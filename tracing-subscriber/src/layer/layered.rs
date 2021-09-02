use tracing_core::{
    metadata::Metadata,
    span,
    subscriber::{Interest, Subscriber},
    Event, LevelFilter,
};

#[cfg(feature = "registry")]
use crate::registry::Registry;
use crate::{
    filter::{self, FilterId},
    layer::{Context, Layer},
    registry::LookupSpan,
};
use std::{
    any::{type_name, TypeId},
    fmt,
    marker::PhantomData,
};

/// A [`Subscriber`] composed of a `Subscriber` wrapped by one or more
/// [`Layer`]s.
///
/// [`Layer`]: crate::Layer
/// [`Subscriber`]: https://docs.rs/tracing-core/latest/tracing_core/trait.Subscriber.html
#[derive(Clone)]
pub struct Layered<L, I, S = I> {
    /// The layer.
    layer: L,

    /// The inner value that `self.layer` was layered onto.
    ///
    /// If this is also a `Layer`, then this `Layered` will implement `Layer`.
    /// If this is a `Subscriber`, then this `Layered` will implement
    /// `Subscriber` instead.
    inner: I,

    // These booleans are used to determine how to combine `Interest`s and max
    // level hints when per-layer filters are in use.
    /// Is `self.inner` a `Registry`?
    ///
    /// If so, when combining `Interest`s, we want to "bubble up" its
    /// `Interest`.
    inner_is_registry: bool,

    /// Does `self.layer` have per-layer filters?
    ///
    /// This will be true if:
    /// - `self.inner` is a `Filtered`.
    /// - `self.inner` is a tree of `Layered`s where _all_ arms of those
    ///   `Layered`s have per-layer filters.
    ///
    /// Otherwise, if it's a `Layered` with one per-layer filter in one branch,
    /// but a non-per-layer-filtered layer in the other branch, this will be
    /// _false_, because the `Layered` is already handling the combining of
    /// per-layer filter `Interest`s and max level hints with its non-filtered
    /// `Layer`.
    has_layer_filter: bool,

    /// Does `self.inner` have per-layer filters?
    ///
    /// This is determined according to the same rules as
    /// `has_layer_filter` above.
    inner_has_layer_filter: bool,
    _s: PhantomData<fn(S)>,
}

// === impl Layered ===

impl<L, S> Subscriber for Layered<L, S>
where
    L: Layer<S>,
    S: Subscriber,
{
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.pick_interest(self.layer.register_callsite(metadata), || {
            self.inner.register_callsite(metadata)
        })
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        if self.layer.enabled(metadata, self.ctx()) {
            // if the outer layer enables the callsite metadata, ask the subscriber.
            self.inner.enabled(metadata)
        } else {
            // otherwise, the callsite is disabled by the layer
            false
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.pick_level_hint(self.layer.max_level_hint(), self.inner.max_level_hint())
    }

    fn new_span(&self, span: &span::Attributes<'_>) -> span::Id {
        let id = self.inner.new_span(span);
        self.layer.new_span(span, &id, self.ctx());
        id
    }

    fn record(&self, span: &span::Id, values: &span::Record<'_>) {
        self.inner.record(span, values);
        self.layer.on_record(span, values, self.ctx());
    }

    fn record_follows_from(&self, span: &span::Id, follows: &span::Id) {
        self.inner.record_follows_from(span, follows);
        self.layer.on_follows_from(span, follows, self.ctx());
    }

    fn event(&self, event: &Event<'_>) {
        self.inner.event(event);
        self.layer.on_event(event, self.ctx());
    }

    fn enter(&self, span: &span::Id) {
        self.inner.enter(span);
        self.layer.on_enter(span, self.ctx());
    }

    fn exit(&self, span: &span::Id) {
        self.inner.exit(span);
        self.layer.on_exit(span, self.ctx());
    }

    fn clone_span(&self, old: &span::Id) -> span::Id {
        let new = self.inner.clone_span(old);
        if &new != old {
            self.layer.on_id_change(old, &new, self.ctx())
        };
        new
    }

    #[inline]
    fn drop_span(&self, id: span::Id) {
        self.try_close(id);
    }

    fn try_close(&self, id: span::Id) -> bool {
        #[cfg(feature = "registry")]
        let subscriber = &self.inner as &dyn Subscriber;
        #[cfg(feature = "registry")]
        let mut guard = subscriber
            .downcast_ref::<Registry>()
            .map(|registry| registry.start_close(id.clone()));
        if self.inner.try_close(id.clone()) {
            // If we have a registry's close guard, indicate that the span is
            // closing.
            #[cfg(feature = "registry")]
            {
                if let Some(g) = guard.as_mut() {
                    g.is_closing()
                };
            }

            self.layer.on_close(id, self.ctx());
            true
        } else {
            false
        }
    }

    #[inline]
    fn current_span(&self) -> span::Current {
        self.inner.current_span()
    }

    #[doc(hidden)]
    unsafe fn downcast_raw(&self, id: TypeId) -> Option<*const ()> {
        // If downcasting to `Self`, return a pointer to `self`.
        if id == TypeId::of::<Self>() {
            return Some(self as *const _ as *const ());
        }

        self.layer
            .downcast_raw(id)
            .or_else(|| self.inner.downcast_raw(id))
    }
}

impl<S, A, B> Layer<S> for Layered<A, B, S>
where
    A: Layer<S>,
    B: Layer<S>,
    S: Subscriber,
{
    fn on_layer(&mut self, subscriber: &mut S) {
        self.layer.on_layer(subscriber);
        self.inner.on_layer(subscriber);
    }

    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.pick_interest(self.layer.register_callsite(metadata), || {
            self.inner.register_callsite(metadata)
        })
    }

    fn enabled(&self, metadata: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        if self.layer.enabled(metadata, ctx.clone()) {
            // if the outer subscriber enables the callsite metadata, ask the inner layer.
            self.inner.enabled(metadata, ctx)
        } else {
            // otherwise, the callsite is disabled by this layer
            false
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.pick_level_hint(self.layer.max_level_hint(), self.inner.max_level_hint())
    }

    #[inline]
    fn new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.new_span(attrs, id, ctx.clone());
        self.layer.new_span(attrs, id, ctx);
    }

    #[inline]
    fn on_record(&self, span: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        self.inner.on_record(span, values, ctx.clone());
        self.layer.on_record(span, values, ctx);
    }

    #[inline]
    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_follows_from(span, follows, ctx.clone());
        self.layer.on_follows_from(span, follows, ctx);
    }

    #[inline]
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        self.inner.on_event(event, ctx.clone());
        self.layer.on_event(event, ctx);
    }

    #[inline]
    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_enter(id, ctx.clone());
        self.layer.on_enter(id, ctx);
    }

    #[inline]
    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_exit(id, ctx.clone());
        self.layer.on_exit(id, ctx);
    }

    #[inline]
    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        self.inner.on_close(id.clone(), ctx.clone());
        self.layer.on_close(id, ctx);
    }

    #[inline]
    fn on_id_change(&self, old: &span::Id, new: &span::Id, ctx: Context<'_, S>) {
        self.inner.on_id_change(old, new, ctx.clone());
        self.layer.on_id_change(old, new, ctx);
    }

    #[doc(hidden)]
    unsafe fn downcast_raw(&self, id: TypeId) -> Option<*const ()> {
        match id {
            // If downcasting to `Self`, return a pointer to `self`.
            id if id == TypeId::of::<Self>() => Some(self as *const _ as *const ()),

            // Oh, we're looking for per-layer filters!
            //
            // This should only happen if we are inside of another `Layered`,
            // and it's trying to determine how it should combine `Interest`s
            // and max level hints.
            //
            // In that case, we should be considered to be "per-layer filtered"
            // if *both* the outer layer and the inner layer/subscriber have
            // per-layer filters. Otherwise, we should be considered to *not* be
            // per-layer filtered (even if one or the other has per layer
            // filters). This is because *we* are expected to handle the
            // `Interest`/level hint combining at this level, and will be
            // propagating a non-PLF hint/interest.
            //
            // Yes, this rule *is* slightly counter-intuitive, but it's
            // necessary due to a weird edge case that can occur when two
            // `Layered`s where one side is per-layer filtered and the other
            // isn't are `Layered` together to form a tree. If we didn't have
            // this rule, we would actually end up *ignoring* `Interest`s from
            // the non-per-layer-filtered layers, since both branches would
            // claim to have PLF.
            //
            // If you don't understand this...that's fine, just don't mess with
            // it. :)
            id if id == TypeId::of::<filter::MagicPlfDowncastMarker>() => {
                self.layer.downcast_raw(id).and(self.inner.downcast_raw(id))
            }

            // Otherwise, try to downcast both branches normally...
            _ => self
                .layer
                .downcast_raw(id)
                .or_else(|| self.inner.downcast_raw(id)),
        }
    }
}

impl<L, S> Layer<S> for Option<L>
where
    L: Layer<S>,
    S: Subscriber,
{
    fn on_layer(&mut self, subscriber: &mut S) {
        if let Some(ref mut layer) = self {
            layer.on_layer(subscriber)
        }
    }

    #[inline]
    fn new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.new_span(attrs, id, ctx)
        }
    }

    #[inline]
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        match self {
            Some(ref inner) => inner.register_callsite(metadata),
            None => Interest::always(),
        }
    }

    #[inline]
    fn enabled(&self, metadata: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        match self {
            Some(ref inner) => inner.enabled(metadata, ctx),
            None => true,
        }
    }

    #[inline]
    fn max_level_hint(&self) -> Option<LevelFilter> {
        match self {
            Some(ref inner) => inner.max_level_hint(),
            None => None,
        }
    }

    // #[doc(hidden)]
    // const HAS_PER_LAYER_FILTERS: bool = L::HAS_PER_LAYER_FILTERS;

    #[inline]
    fn on_record(&self, span: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.on_record(span, values, ctx);
        }
    }

    #[inline]
    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.on_follows_from(span, follows, ctx);
        }
    }

    #[inline]
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.on_event(event, ctx);
        }
    }

    #[inline]
    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.on_enter(id, ctx);
        }
    }

    #[inline]
    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.on_exit(id, ctx);
        }
    }

    #[inline]
    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.on_close(id, ctx);
        }
    }

    #[inline]
    fn on_id_change(&self, old: &span::Id, new: &span::Id, ctx: Context<'_, S>) {
        if let Some(ref inner) = self {
            inner.on_id_change(old, new, ctx)
        }
    }

    #[doc(hidden)]
    #[inline]
    unsafe fn downcast_raw(&self, id: TypeId) -> Option<*const ()> {
        if id == TypeId::of::<Self>() {
            Some(self as *const _ as *const ())
        } else {
            self.as_ref().and_then(|inner| inner.downcast_raw(id))
        }
    }
}

impl<'a, L, S> LookupSpan<'a> for Layered<L, S>
where
    S: Subscriber + LookupSpan<'a>,
{
    type Data = S::Data;

    fn span_data(&'a self, id: &span::Id) -> Option<Self::Data> {
        self.inner.span_data(id)
    }

    fn register_filter(&mut self) -> FilterId {
        self.inner.register_filter()
    }
}

impl<L, S> Layered<L, S>
where
    S: Subscriber,
{
    fn ctx(&self) -> Context<'_, S> {
        Context::new(&self.inner)
    }
}

impl<A, B, S> Layered<A, B, S>
where
    A: Layer<S>,
    S: Subscriber,
{
    pub(super) fn new(layer: A, inner: B, inner_has_layer_filter: bool) -> Self {
        #[cfg(feature = "registry")]
        let inner_is_registry = TypeId::of::<S>() == TypeId::of::<crate::registry::Registry>();
        #[cfg(not(feature = "registry"))]
        let inner_is_registry = false;

        let inner_has_layer_filter = inner_has_layer_filter || inner_is_registry;
        let has_layer_filter = filter::layer_has_plf(&layer);
        Self {
            layer,
            inner,
            has_layer_filter,
            inner_has_layer_filter,
            inner_is_registry,
            _s: PhantomData,
        }
    }

    fn pick_interest(&self, outer: Interest, inner: impl FnOnce() -> Interest) -> Interest {
        if self.has_layer_filter {
            return inner();
        }

        // If the outer layer has disabled the callsite, return now so that
        // the inner layer/subscriber doesn't get its hopes up.
        if outer.is_never() {
            return outer;
        }

        // The intention behind calling `inner.register_callsite()` before the if statement
        // is to ensure that the inner subscriber is informed that the callsite exists
        // regardless of the outer layer's filtering decision.
        let inner = inner();
        if outer.is_sometimes() {
            // if this interest is "sometimes", return "sometimes" to ensure that
            // filters are reevaluated.
            return outer;
        }

        if inner.is_never() && !outer.is_never() && self.inner_has_layer_filter {
            return Interest::sometimes();
        }

        // otherwise, allow the inner subscriber or collector to weigh in.
        inner
    }

    fn pick_level_hint(
        &self,
        outer_hint: Option<LevelFilter>,
        inner_hint: Option<LevelFilter>,
    ) -> Option<LevelFilter> {
        use std::cmp::max;

        if self.inner_is_registry {
            return outer_hint;
        }

        if self.has_layer_filter && self.inner_has_layer_filter {
            return Some(max(outer_hint?, inner_hint?));
        }

        if self.has_layer_filter && inner_hint.is_none() {
            return None;
        }

        if self.inner_has_layer_filter && outer_hint.is_none() {
            return None;
        }

        max(outer_hint, inner_hint)
    }
}

impl<A, B, S> fmt::Debug for Layered<A, B, S>
where
    A: fmt::Debug,
    B: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let alt = f.alternate();
        let mut s = f.debug_struct("Layered");
        s.field("layer", &self.layer).field("inner", &self.inner);

        // These additional fields are more verbose and usually only necessary
        // for internal debugging purposes, so only print them if alternate mode
        // is enabled.
        if alt {
            s.field("inner_is_registry", &self.inner_is_registry)
                .field("has_layer_filter", &self.has_layer_filter)
                .field("inner_has_layer_filter", &self.inner_has_layer_filter)
                .field(
                    "subscriber",
                    &format_args!("PhantomData<{}>", type_name::<S>()),
                );
        }

        s.finish()
    }
}
