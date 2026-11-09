use futures::future::BoxFuture;

use super::{BinaryMatcher, Matcher};
use crate::resolvers::{AsyncContext, Result};

struct Transform<'scope, I, O, F> {
    matcher: Matcher<'scope, I, O>,
    func: F,
}

impl<'scope, I: Send + 'scope, O: Send + 'scope, U: Send, F> super::pipeline::Run<I, U>
    for Transform<'scope, I, O, F>
where
    F: Fn((&AsyncContext<'_>, O)) -> Result<U> + Send + Sync,
{
    fn run<'ctx, 'image>(
        &'ctx self,
        input: I,
        ctx: &'ctx AsyncContext<'image>,
    ) -> BoxFuture<'ctx, Result<U>>
    where
        I: 'ctx,
        U: 'ctx,
    {
        Box::pin(async move { (self.func)((ctx, self.matcher.run(input, ctx).await?)) })
    }
}

struct Resolve;

impl<'scope> super::pipeline::Run<BinaryMatcher<'scope>, Vec<u64>> for Resolve {
    fn run<'ctx, 'image>(
        &'ctx self,
        matcher: BinaryMatcher<'scope>,
        ctx: &'ctx AsyncContext<'image>,
    ) -> BoxFuture<'ctx, Result<Vec<u64>>>
    where
        BinaryMatcher<'scope>: 'ctx,
    {
        Box::pin(async move { matcher.resolve(ctx).await })
    }
}

/// Supply `(ctx, value)` to one synchronous transformation, then return a plain matcher.
/// Context is borrowed from each run and cannot escape in the output.
///
/// ```
/// use patternsleuth::{binary_matchers::BinaryMatcher, MemoryTrait};
/// let matcher = BinaryMatcher::address(0x1000)
///     .with_context()
///     .try_map_each(|(ctx, address)| Ok(ctx.image().memory.u8(address)?))
///     .map_each(u64::from);
/// ```
///
/// The output cannot retain the run's context:
///
/// ```compile_fail
/// use patternsleuth::binary_matchers::BinaryMatcher;
/// let invalid = BinaryMatcher::address(0x1000)
///     .with_context()
///     .map(|(ctx, _)| ctx.clone());
/// ```
///
/// The adapter is consumed by one transformation; subsequent callbacks are data-only:
///
/// ```compile_fail
/// use patternsleuth::binary_matchers::BinaryMatcher;
/// let invalid = BinaryMatcher::address(0x1000)
///     .with_context().map_each(|(_, address)| address)
///     .map_each(|(ctx, address)| address);
/// ```
#[must_use = "with_context applies only when followed by a transformation"]
pub struct WithContext<'scope, I, O> {
    pub(super) matcher: Matcher<'scope, I, O>,
}

impl<'scope, I: Send + 'scope, O: Send + 'scope> WithContext<'scope, I, O> {
    /// Transform the whole output with `(ctx, output)`.
    pub fn map<U, F>(self, func: F) -> Matcher<'scope, I, U>
    where
        U: Send + 'scope,
        F: Fn((&AsyncContext<'_>, O)) -> U + Send + Sync + 'scope,
    {
        self.try_map(move |input| Ok(func(input)))
    }

    /// Fallibly transform the whole output with `(ctx, output)`.
    pub fn try_map<U, F>(self, func: F) -> Matcher<'scope, I, U>
    where
        U: Send + 'scope,
        F: Fn((&AsyncContext<'_>, O)) -> Result<U> + Send + Sync + 'scope,
    {
        Matcher::from_run(Transform {
            matcher: self.matcher,
            func,
        })
    }
}

impl<'scope, I: Send + 'scope, T: Send + 'scope> WithContext<'scope, I, Vec<T>> {
    /// Transform each item with `(ctx, item)`; skip callbacks on empty input.
    pub fn map_each<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn((&AsyncContext<'_>, T)) -> U + Send + Sync + 'scope,
    {
        self.try_map_each(move |input| Ok(func(input)))
    }

    /// Fallibly transform items in order, stopping at the first error.
    pub fn try_map_each<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn((&AsyncContext<'_>, T)) -> Result<U> + Send + Sync + 'scope,
    {
        self.try_map(move |(ctx, items)| items.into_iter().map(|item| func((ctx, item))).collect())
    }

    /// Expand each item with `(ctx, item)`, retaining order and duplicates.
    pub fn flat_map<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn((&AsyncContext<'_>, T)) -> Vec<U> + Send + Sync + 'scope,
    {
        self.try_flat_map(move |input| Ok(func(input)))
    }

    pub fn try_flat_map<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn((&AsyncContext<'_>, T)) -> Result<Vec<U>> + Send + Sync + 'scope,
    {
        self.try_map(move |(ctx, items)| {
            let mut result = Vec::new();
            for item in items {
                result.extend(func((ctx, item))?);
            }
            Ok(result)
        })
    }

    /// Keep items matching `(ctx, &item)`, without copying or cloning items.
    pub fn filter_each<F>(self, predicate: F) -> Matcher<'scope, I, Vec<T>>
    where
        F: Fn((&AsyncContext<'_>, &T)) -> bool + Send + Sync + 'scope,
    {
        self.try_filter(move |input| Ok(predicate(input)))
    }

    /// Fallible borrowed predicate receiving `(ctx, &item)`.
    pub fn try_filter<F>(self, predicate: F) -> Matcher<'scope, I, Vec<T>>
    where
        F: Fn((&AsyncContext<'_>, &T)) -> Result<bool> + Send + Sync + 'scope,
    {
        self.filter_modify(move |(ctx, item)| Ok(predicate((ctx, &item))?.then_some(item)))
    }

    /// Some keeps a transformed item; None drops it; the first error stops evaluation.
    pub fn filter_modify<U, F>(self, func: F) -> Matcher<'scope, I, Vec<U>>
    where
        U: Send + 'scope,
        F: Fn((&AsyncContext<'_>, T)) -> Result<Option<U>> + Send + Sync + 'scope,
    {
        self.try_map(move |(ctx, items)| {
            let mut result = Vec::new();
            for item in items {
                if let Some(item) = func((ctx, item))? {
                    result.push(item);
                }
            }
            Ok(result)
        })
    }
}

impl<'scope, I: Send + 'scope, T: Copy + Send + 'scope> WithContext<'scope, I, Vec<T>> {
    /// By-value predicate receiving `(ctx, item)` for copyable items.
    pub fn filter<F>(self, predicate: F) -> Matcher<'scope, I, Vec<T>>
    where
        F: Fn((&AsyncContext<'_>, T)) -> bool + Send + Sync + 'scope,
    {
        self.filter_each(move |(ctx, item)| predicate((ctx, *item)))
    }
}

impl<'scope, I: Send + 'scope> WithContext<'scope, I, Vec<u64>> {
    /// Transform each item into an absolute address using `(ctx, address)`.
    pub fn modify_with<F>(self, func: F) -> Matcher<'scope, I, Vec<u64>>
    where
        F: Fn((&AsyncContext<'_>, u64)) -> Result<u64> + Send + Sync + 'scope,
    {
        self.try_map_each(func)
    }
}

impl<'scope> WithContext<'scope, (), Vec<u64>> {
    /// Build and resolve a new source from `(ctx, addresses)`, even for empty input.
    pub fn sub<F>(self, func: F) -> BinaryMatcher<'scope>
    where
        F: Fn((&AsyncContext<'_>, Vec<u64>)) -> Result<BinaryMatcher<'scope>>
            + Send
            + Sync
            + 'scope,
    {
        self.try_map(func).then(Matcher::from_run(Resolve))
    }
}
