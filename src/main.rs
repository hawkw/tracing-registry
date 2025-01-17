use serde_json::{json, Value};
use sharded_slab::{Guard, Slab};

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::Id;
use tracing::{info, span, Event, Level, Metadata};
use tracing_core::{Interest, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

#[derive(Debug, Default)]
pub struct StdoutLayer<S: Subscriber> {
    inner: S,
}

#[derive(Debug, Default)]
pub struct StderrLayer {}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for StdoutLayer<S> {
    fn on_close(&self, id: Id, ctx: Context<S>) {
        let span = self.inner.span(&id);
        dbg!(span);
    }
}

impl<S: Subscriber> Layer<S> for StderrLayer {}

pub trait LookupSpan<'a> {
    type Span: SpanData<'a> + fmt::Debug;
    fn span(&'a self, id: &Id) -> Option<Self::Span>;
}

pub trait SpanData<'a> {
    type Children: Iterator<Item = &'a Id>;
    type Follows: Iterator<Item = &'a Id>;

    fn id(&self) -> &Id;
    fn metadata(&self) -> &'static Metadata<'static>;
    fn parent(&self) -> Option<&Id>;
    fn children(&'a self) -> Self::Children;
    fn follows_from(&'a self) -> Self::Follows;
}

struct RegistryVisitor<'a>(&'a mut HashMap<&'static str, Value>);

impl<'a> Visit for RegistryVisitor<'a> {
    // TODO: special visitors for various formats that honeycomb.io supports
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let s = format!("{:?}", value);
        self.0.insert(field.name(), json!(s));
    }
}

impl<'a> SpanData<'a> for Guard<'a, BigSpan> {
    type Children = std::slice::Iter<'a, Id>; // not yet implemented...
    type Follows = std::slice::Iter<'a, Id>;

    fn id(&self) -> &Id {
        unimplemented!("david: add this to `BigSpan`")
    }
    fn metadata(&self) -> &'static Metadata<'static> {
        (*self).metadata
    }
    fn parent(&self) -> Option<&Id> {
        unimplemented!("david: add this to `BigSpan`")
    }
    fn children(&self) -> Self::Children {
        unimplemented!("david: add this to `BigSpan`")
    }
    fn follows_from(&self) -> Self::Follows {
        unimplemented!("david: add this to `BigSpan`")
    }
}

#[derive(Debug)]
struct BigSpan {
    metadata: &'static Metadata<'static>,
    values: Mutex<HashMap<&'static str, Value>>,
    events: Mutex<Vec<BigEvent>>,
}

#[derive(Debug)]
struct BigEvent {
    parent: Id,
    metadata: &'static Metadata<'static>,
    values: HashMap<&'static str, Value>,
}

// XXX(eliza): should this have a SpanData bound? The expectation is that there
// would be add'l impls for `T: LookupSpan where T::Span: Extensions`...
//
// XXX(eliza): also, consider having `.extensions`/`.extensions_mut` methods to
// get the extensions, so we can control read locking vs write locking?
pub trait Extensions {
    fn get<T: Any>(&self) -> Option<&T>;
    fn get_mut<T: Any>(&mut self) -> Option<&mut T>;
    fn insert<T: Any>(&mut self, t: T) -> Option<T>;
    fn remove<T: Any>(&mut self) -> Option<T>;
}

#[derive(Debug)]
struct Registry {
    spans: Arc<Slab<BigSpan>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            spans: Arc::new(Slab::new()),
        }
    }
}

fn convert_id(id: Id) -> usize {
    let id: usize = id.into_u64().try_into().unwrap();
    id - 1
}

impl Registry {
    fn insert(&self, s: BigSpan) -> Option<usize> {
        self.spans.insert(s)
    }

    fn get(&self, id: &Id) -> Option<Guard<BigSpan>> {
        self.spans.get(convert_id(id.clone()))
    }

    fn take(&self, id: Id) -> Option<BigSpan> {
        let id = convert_id(id);
        self.spans.take(id)
    }
}

thread_local! {
    pub static CURRENT_SPAN: Cell<Option<u64>> = Cell::new(Some(1));
}

impl Subscriber for Registry {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    #[inline]
    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let mut values = HashMap::new();
        let mut visitor = RegistryVisitor(&mut values);
        attrs.record(&mut visitor);
        let s = BigSpan {
            metadata: attrs.metadata(),
            values: Mutex::new(values),
            events: Mutex::new(vec![]),
        };
        let id = (self.insert(s).expect("Unable to allocate another span") + 1) as u64;
        Id::from_u64(id.try_into().unwrap())
    }

    #[inline]
    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {
        // self.spans.record(span, values, &self.fmt_fields)
        // unimplemented!()
    }

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {
        // TODO: implement this please
    }

    fn enter(&self, id: &span::Id) {
        let id = id.into_u64();
        CURRENT_SPAN.with(|s| s.set(Some(id)));
    }

    fn event(&self, event: &Event<'_>) {
        let id = match event.parent() {
            Some(id) => Some(id.clone()),
            None => {
                if event.is_contextual() {
                    let id = CURRENT_SPAN.with(|s| s.get());
                    let id = id.expect("Contextual span ID not found");
                    Some(span::Id::from_u64(id))
                } else {
                    None
                }
            }
        };
        if let Some(id) = id {
            let mut values = HashMap::new();
            let mut visitor = RegistryVisitor(&mut values);
            event.record(&mut visitor);
            let span = self.get(&id).expect("Missing parent span for event");
            let event = BigEvent {
                parent: id,
                metadata: event.metadata(),
                values,
            };
            span.events.lock().expect("Mutex poisoned").push(event);
        }
    }

    fn exit(&self, _id: &span::Id) {
        CURRENT_SPAN.with(|s| s.take());
    }

    #[inline]
    fn try_close(&self, id: span::Id) -> bool {
        let span = self.take(id);
        // dbg!(span);
        true
    }
}

impl<'a> LookupSpan<'a> for Registry {
    type Span = Guard<'a, BigSpan>;

    fn span(&'a self, id: &Id) -> Option<Self::Span> {
        self.get(id)
    }
}

fn main() {
    let subscriber = Registry::default();
    tracing::subscriber::set_global_default(subscriber).expect("Could not set global default");

    let span = span!(Level::INFO, "my_loop");
    let _entered = span.enter();
    for i in 0..10 {
        span!(Level::INFO, "iteration").in_scope(|| info!(iteration = i, "In a span!"));
    }
}
