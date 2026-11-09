use std::sync::Arc;

use futures::future::BoxFuture;

use super::WithContext;
use crate::resolvers::{AsyncContext, Result};

pub(super) trait Run<I, O>: Send + Sync {
    fn run<'ctx, 'image>(
        &'ctx self,
        input: I,
        ctx: &'ctx AsyncContext<'image>,
    ) -> BoxFuture<'ctx, Result<O>>
    where
        I: 'ctx,
        O: 'ctx;
}

struct AsyncStage<F>(F);

impl<I, O, F> Run<I, O> for AsyncStage<F>
where
    F: for<'ctx, 'image> Fn(I, &'ctx AsyncContext<'image>) -> BoxFuture<'ctx, Result<O>>
        + Send
        + Sync,
{
    fn run<'ctx, 'image>(
        &'ctx self,
        input: I,
        ctx: &'ctx AsyncContext<'image>,
    ) -> BoxFuture<'ctx, Result<O>>
    where
        I: 'ctx,
        O: 'ctx,
    {
        (self.0)(input, ctx)
    }
}

struct Identity;

impl<T: Send> Run<T, T> for Identity {
    fn run<'ctx, 'image>(
        &'ctx self,
        input: T,
        _: &'ctx AsyncContext<'image>,
    ) -> BoxFuture<'ctx, Result<T>>
    where
        T: 'ctx,
    {
        Box::pin(async move { Ok(input) })
    }
}

struct Then<'scope, I, O, T> {
    first: Matcher<'scope, I, O>,
    next: Matcher<'scope, O, T>,
}

impl<'scope, I: Send + 'scope, O: Send + 'scope, T: Send + 'scope> Run<I, T>
    for Then<'scope, I, O, T>
{
    fn run<'ctx, 'image>(
        &'ctx self,
        input: I,
        ctx: &'ctx AsyncContext<'image>,
    ) -> BoxFuture<'ctx, Result<T>>
    where
        I: 'ctx,
        T: 'ctx,
    {
        Box::pin(async move { self.next.run(self.first.run(input, ctx).await?, ctx).await })
    }
}

struct Fork<'scope, I, O, T> {
    left: Matcher<'scope, I, O>,
    right: Matcher<'scope, I, T>,
}

impl<'scope, I: Clone + Send + 'scope, O: Send + 'scope, T: Send + 'scope> Run<I, (O, T)>
    for Fork<'scope, I, O, T>
{
    fn run<'ctx, 'image>(
        &'ctx self,
        input: I,
        ctx: &'ctx AsyncContext<'image>,
    ) -> BoxFuture<'ctx, Result<(O, T)>>
    where
        I: 'ctx,
        (O, T): 'ctx,
    {
        Box::pin(async move {
            let left = self.left.run(input.clone(), ctx).await?;
            let right = self.right.run(input, ctx).await?;
            Ok((left, right))
        })
    }
}

/// A reusable, fallible stage from I to O, with read-only image context.
/// Cloning shares the stage, not its results; each run evaluates it again.
/// Inputs, outputs, and transformation captures may borrow for `'scope`.
/// Each evaluation borrows its context independently.
///
/// ```
/// use patternsleuth::binary_matchers::Matcher;
/// fn strip_prefix<'a>(prefix: &'a str) -> Matcher<'a, &'a str, &'a str> {
///     Matcher::new::<&str>()
///         .map(move |value| value.strip_prefix(prefix).unwrap_or(value))
/// }
/// ```
///
/// A stage cannot outlive its captured data:
///
/// ```compile_fail
/// use patternsleuth::binary_matchers::Matcher;
/// let stage;
/// {
///     let prefix = String::from("prefix:");
///     stage = Matcher::new::<&str>()
///         .map(|value| value.strip_prefix(prefix.as_str()).unwrap_or(value));
/// }
/// drop(stage);
/// ```
///
/// Whole-state composition checks adjacent input/output types:
///
/// ```
/// use patternsleuth::binary_matchers::Matcher;
/// let stage = Matcher::<u64, u64>::identity()
///     .map(|address| format!("{address:#x}"))
///     .then(Matcher::<String, String>::identity());
/// ```
///
/// Incompatible stages do not compose:
///
/// ```compile_fail
/// use patternsleuth::binary_matchers::Matcher;
/// let stage = Matcher::<u64, u64>::identity()
///     .map(|address| address.to_string());
/// let invalid = stage.then(Matcher::<Vec<u64>, Vec<u64>>::identity());
/// ```
///
/// Ordinary transformations receive data only; image access is explicit:
///
/// ```
/// use patternsleuth::{binary_matchers::BinaryMatcher, MemoryTrait};
/// let stage = BinaryMatcher::address(0x1000)
///     .map_each(|address| address + 1)
///     .with_context()
///     .try_map_each(|(ctx, address)| Ok(ctx.image().memory.u8(address)?))
///     .map(|bytes| bytes.len());
/// ```
///
/// ```compile_fail
/// use patternsleuth::binary_matchers::Matcher;
/// let invalid = Matcher::<u64, u64>::identity().map(|value, ctx| value);
/// ```
#[must_use = "matchers do nothing until run or resolved"]
pub struct Matcher<'scope, I, O> {
    run: Arc<dyn Run<I, O> + 'scope>,
}

impl<'scope, I, O> Clone for Matcher<'scope, I, O> {
    fn clone(&self) -> Self {
        Self {
            run: Arc::clone(&self.run),
        }
    }
}

impl<'scope, I, O> std::fmt::Display for Matcher<'scope, I, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Matcher")
    }
}

impl<'scope, I: Send + 'scope, O: Send + 'scope> Matcher<'scope, I, O> {
    /// Build an asynchronous stage. The future may borrow ctx, but owns its input.
    /// The callback must support every context lifetime; for borrowed pipeline
    /// data, use `new` and the transformation methods instead.
    pub fn from_async<F>(run: F) -> Self
    where
        F: for<'ctx, 'image> Fn(I, &'ctx AsyncContext<'image>) -> BoxFuture<'ctx, Result<O>>
            + Send
            + Sync
            + 'scope,
    {
        Self::from_run(AsyncStage(run))
    }

    /// Build a fallible synchronous stage from a data-only callback.
    ///
    /// ```
    /// use patternsleuth::binary_matchers::Matcher;
    /// let count = Matcher::from_sync(|addresses: Vec<u64>| Ok(addresses.len()));
    /// ```
    pub fn from_sync<F>(run: F) -> Matcher<'scope, I, O>
    where
        F: Fn(I) -> Result<O> + Send + Sync + 'scope,
    {
        Matcher::identity().try_map(run)
    }

    pub(super) fn from_run(run: impl Run<I, O> + 'scope) -> Self {
        Self { run: Arc::new(run) }
    }

    pub async fn run(&self, input: I, ctx: &AsyncContext<'_>) -> Result<O> {
        self.run.run(input, ctx).await
    }

    /// Expose `(ctx, value)` to the next transformation only, borrowing this run's context.
    /// Whole-state operations receive the output; per-item operations receive one item.
    pub fn with_context(self) -> WithContext<'scope, I, O> {
        WithContext { matcher: self }
    }

    /// Evaluate this stage once, then pass its output to next.
    pub fn then<T: Send + 'scope>(self, next: Matcher<'scope, O, T>) -> Matcher<'scope, I, T> {
        Matcher::from_run(Then { first: self, next })
    }

    /// Transform the whole output state, not each collection item.
    pub fn map<T, F>(self, func: F) -> Matcher<'scope, I, T>
    where
        T: Send + 'scope,
        F: Fn(O) -> T + Send + Sync + 'scope,
    {
        self.try_map(move |output| Ok(func(output)))
    }

    /// Fallibly transform the whole output state.
    pub fn try_map<T, F>(self, func: F) -> Matcher<'scope, I, T>
    where
        T: Send + 'scope,
        F: Fn(O) -> Result<T> + Send + Sync + 'scope,
    {
        self.with_context().try_map(move |(_, output)| func(output))
    }

    /// Run two stages on the same input, left first. The first error stops evaluation.
    /// Place fork after a shared upstream stage to avoid evaluating that stage twice.
    pub fn fork<T: Send + 'scope>(self, right: Matcher<'scope, I, T>) -> Matcher<'scope, I, (O, T)>
    where
        I: Clone,
    {
        Matcher::from_run(Fork { left: self, right })
    }
}

impl<'scope> Matcher<'scope, (), ()> {
    /// Start a stage with the caller's input, passing it through unchanged.
    /// Subsequent transformations infer their output types.
    ///
    /// ```
    /// use patternsleuth::binary_matchers::Matcher;
    /// let count = Matcher::new::<Vec<u64>>()
    ///     .map(|addresses| addresses.len());
    /// // Matcher<Vec<u64>, usize>
    /// ```
    pub fn new<Input: Send + 'scope>() -> Matcher<'scope, Input, Input> {
        Matcher::identity()
    }
}

impl<'scope, T: Send + 'scope> Matcher<'scope, T, T> {
    pub fn identity() -> Self {
        Self::from_run(Identity)
    }
}

impl<'scope, O: Send + 'scope> Matcher<'scope, (), O> {
    pub async fn resolve(&self, ctx: &AsyncContext<'_>) -> Result<O> {
        self.run((), ctx).await
    }
}

impl<'scope, I: Send + 'scope, T: Send + 'scope> Matcher<'scope, I, Vec<T>> {
    /// Transform items independently, in order. Empty input skips the callback.
    pub fn map_each<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn(T) -> U + Send + Sync + 'scope,
    {
        self.try_map_each(move |item| Ok(func(item)))
    }

    /// Fallibly transform each item; the first error skips remaining callbacks.
    pub fn try_map_each<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn(T) -> Result<U> + Send + Sync + 'scope,
    {
        self.try_map(move |items| items.into_iter().map(&func).collect())
    }

    /// Expand each item into zero or more items, preserving order and duplicates.
    pub fn flat_map<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn(T) -> Vec<U> + Send + Sync + 'scope,
    {
        self.try_flat_map(move |item| Ok(func(item)))
    }

    /// Fallibly expand each item; the first error stops evaluation.
    pub fn try_flat_map<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn(T) -> Result<Vec<U>> + Send + Sync + 'scope,
    {
        self.try_map(move |items| {
            let mut result = Vec::new();
            for item in items {
                result.extend(func(item)?);
            }
            Ok(result)
        })
    }

    /// Keep items matching the predicate, in order. The predicate borrows each item.
    pub fn filter_each<F>(self, predicate: F) -> Self
    where
        F: Fn(&T) -> bool + Send + Sync + 'scope,
    {
        self.try_filter(move |item| Ok(predicate(item)))
    }

    /// Fallible borrowed predicate, preserving order and duplicates.
    pub fn try_filter<F>(self, predicate: F) -> Self
    where
        F: Fn(&T) -> Result<bool> + Send + Sync + 'scope,
    {
        self.filter_modify(move |item| Ok(predicate(&item)?.then_some(item)))
    }

    /// Some keeps a transformed item; None drops it; the first error stops evaluation.
    pub fn filter_modify<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn(T) -> Result<Option<U>> + Send + Sync + 'scope,
    {
        self.try_map(move |items| {
            let mut result = Vec::new();
            for item in items {
                if let Some(item) = func(item)? {
                    result.push(item);
                }
            }
            Ok(result)
        })
    }

    /// Keep the first item, or an empty vector. Evaluates the entire upstream stage.
    pub fn first(self) -> Self {
        self.map(|items| items.into_iter().take(1).collect())
    }

    /// Keep the last item, or an empty vector. Evaluates the entire upstream stage.
    pub fn last(self) -> Self {
        self.map(|items| items.into_iter().last().into_iter().collect())
    }

    /// Concatenate both branches, retaining order and duplicates.
    pub fn union(self, other: Self) -> Self
    where
        I: Clone,
    {
        self.fork(other).concat()
    }

    /// Keep left-branch items present in the right branch, retaining left duplicates.
    pub fn intersection(self, other: Self) -> Self
    where
        I: Clone,
        T: PartialEq,
    {
        self.fork(other).map(|(left, right)| {
            left.into_iter()
                .filter(|item| right.contains(item))
                .collect()
        })
    }
}

impl<'scope, I: Send + 'scope, T: Copy + Send + 'scope> Matcher<'scope, I, Vec<T>> {
    /// Keep copyable items matching a predicate that takes each item by value.
    pub fn filter<F>(self, predicate: F) -> Self
    where
        F: Fn(T) -> bool + Send + Sync + 'scope,
    {
        self.filter_each(move |item| predicate(*item))
    }
}

impl<'scope, I: Send + 'scope, T: Send + 'scope> Matcher<'scope, I, (Vec<T>, Vec<T>)> {
    /// Concatenate paired branch outputs, left first, without deduplication.
    pub fn concat(self) -> Matcher<'scope, I, Vec<T>> {
        self.map(|(mut left, right)| {
            left.extend(right);
            left
        })
    }
}
