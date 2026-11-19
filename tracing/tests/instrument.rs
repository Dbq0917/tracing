// These tests require the thread-local scoped dispatcher, which only works when
// we have a standard library. The behaviour being tested should be the same
// with the standard lib disabled.
#![cfg(feature = "std")]

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task,
};

use futures::FutureExt as _;
use tracing::span;
use tracing::{subscriber::with_default, Instrument as _, Level};
use tracing_mock::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[test]
fn span_on_drop() {
    #[derive(Clone, Debug)]
    struct AssertSpanOnDrop;

    impl Drop for AssertSpanOnDrop {
        fn drop(&mut self) {
            tracing::info!("Drop");
        }
    }

    #[allow(dead_code)] // Field not used, but logs on `Drop`
    struct Fut(Option<AssertSpanOnDrop>);

    impl Future for Fut {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, _: &mut task::Context<'_>) -> task::Poll<Self::Output> {
            self.set(Fut(None));
            task::Poll::Ready(())
        }
    }

    let subscriber = subscriber::mock()
        .enter(expect::span().named("foo"))
        .event(
            expect::event()
                .with_ancestry(expect::has_contextual_parent("foo"))
                .at_level(Level::INFO),
        )
        .exit(expect::span().named("foo"))
        .enter(expect::span().named("foo"))
        .exit(expect::span().named("foo"))
        .drop_span(expect::span().named("foo"))
        .enter(expect::span().named("bar"))
        .event(
            expect::event()
                .with_ancestry(expect::has_contextual_parent("bar"))
                .at_level(Level::INFO),
        )
        .exit(expect::span().named("bar"))
        .drop_span(expect::span().named("bar"))
        .only()
        .run();

    with_default(subscriber, || {
        // polled once
        Fut(Some(AssertSpanOnDrop))
            .instrument(tracing::span!(Level::TRACE, "foo"))
            .now_or_never()
            .unwrap();

        // never polled
        drop(Fut(Some(AssertSpanOnDrop)).instrument(tracing::span!(Level::TRACE, "bar")));
    });
}

#[derive(Clone, Default)]
struct SpanTree {
    next_id: Arc<AtomicU64>,
    spans: Arc<Mutex<HashMap<u64, (&'static tracing::Metadata<'static>, Option<u64>)>>>,
    current: Arc<Mutex<Vec<u64>>>,
}

impl tracing::Subscriber for SpanTree {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let parent = if attrs.is_root() {
            None
        } else if attrs.is_contextual() {
            self.current.lock().unwrap().last().copied()
        } else {
            attrs.parent().map(span::Id::into_u64)
        };
        self.spans
            .lock()
            .unwrap()
            .insert(id, (attrs.metadata(), parent));
        span::Id::from_u64(id)
    }

    fn enter(&self, id: &span::Id) {
        self.current.lock().unwrap().push(id.into_u64());
    }

    fn exit(&self, id: &span::Id) {
        let exited = self.current.lock().unwrap().pop();
        assert_eq!(exited, Some(id.into_u64()));
    }

    fn record(&self, _: &span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn current_span(&self) -> tracing_core::span::Current {
        let current = self.current.lock().unwrap().last().copied();
        let spans = self.spans.lock().unwrap();
        current
            .and_then(|id| {
                spans
                    .get(&id)
                    .map(|(meta, _)| tracing_core::span::Current::new(span::Id::from_u64(id), meta))
            })
            .unwrap_or_else(tracing_core::span::Current::none)
    }
}

fn chain(remaining: usize) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let span = match remaining {
            3 => tracing::info_span!("hop3"),
            2 => tracing::info_span!("hop2"),
            1 => tracing::info_span!("hop1"),
            0 => return,
            _ => unreachable!(),
        };
        let next = span.in_scope(|| chain(remaining - 1).in_current_span());
        std::thread::spawn(move || {
            assert!(next.now_or_never().is_some());
        })
        .join()
        .unwrap();
    })
}

#[test]
fn span_context_follows_task_across_three_hops() {
    let subscriber = SpanTree::default();

    with_default(subscriber.clone(), || {
        tracing::info_span!("root").in_scope(|| {
            let mut chain = chain(3);
            assert!(chain.as_mut().now_or_never().is_some());
        });
    });

    let spans = subscriber.spans.lock().unwrap();
    let id_of = |name| {
        spans
            .iter()
            .find_map(|(id, (meta, _))| (meta.name() == name).then_some(*id))
            .unwrap_or_else(|| panic!("missing span `{}`", name))
    };
    let parent_of = |name| spans.get(&id_of(name)).unwrap().1;

    assert_eq!(spans.len(), 4, "expected one root and three hops");
    assert_eq!(
        spans
            .iter()
            .filter(|(_, (_, parent))| parent.is_none())
            .count(),
        1,
        "the task chain must have exactly one root"
    );
    assert_eq!(parent_of("hop3"), Some(id_of("root")));
    assert_eq!(parent_of("hop2"), Some(id_of("hop3")));
    assert_eq!(parent_of("hop1"), Some(id_of("hop2")));
}
