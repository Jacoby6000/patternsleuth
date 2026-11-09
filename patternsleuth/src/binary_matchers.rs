use crate::resolvers::{AsyncContext, ResolveError, Result, unreal::util};
use futures::future::join_all;
use iced_x86::{Decoder, DecoderOptions, FlowControl};
use patternsleuth_scanner::Pattern;
use std::{collections::HashSet, sync::Arc};

mod pipeline;
pub use pipeline::Matcher;
mod context;
pub use context::WithContext;

#[cfg(test)]
mod tests;

/// Address-producing source matcher. Existing fluent address APIs remain available.
/// Construction, callbacks, and resolution use resolvers::Result and ResolveError.
pub type BinaryMatcher<'scope> = Matcher<'scope, (), Vec<u64>>;

/// A parsed pattern or signature string accepted by BinaryMatcher::patterns.
pub trait IntoPattern {
    fn into_pattern(self) -> Result<Pattern>;
}

impl IntoPattern for Pattern {
    fn into_pattern(self) -> Result<Pattern> {
        Ok(self)
    }
}

impl IntoPattern for &Pattern {
    fn into_pattern(self) -> Result<Pattern> {
        Ok(self.clone())
    }
}

impl IntoPattern for &str {
    fn into_pattern(self) -> Result<Pattern> {
        parse_signature(self)
    }
}

impl IntoPattern for String {
    fn into_pattern(self) -> Result<Pattern> {
        parse_signature(self)
    }
}

impl IntoPattern for &String {
    fn into_pattern(self) -> Result<Pattern> {
        parse_signature(self)
    }
}

impl IntoPattern for &&str {
    fn into_pattern(self) -> Result<Pattern> {
        parse_signature(self)
    }
}

fn parse_signature(signature: impl AsRef<str>) -> Result<Pattern> {
    Pattern::new(signature).map_err(|error| ResolveError::new_msg(format!("{error:#}")))
}

/// A known address, not necessarily a function entry or instruction site.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address(pub u64);

/// An instruction's address.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstructionSite(pub u64);

/// A root function's metadata. The end address is exclusive.
/// Bounds describe the root's recorded range, not any separate chained fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionEntry {
    pub start: u64,
    pub end: u64,
    /// Unwind-info address when supplied by the image backend.
    pub unwind: Option<u64>,
}

impl FunctionEntry {
    /// Byte length of the recorded range, not a count of decoded instructions.
    pub fn size(&self) -> u64 {
        self.end - self.start
    }
}

/// A direct near-call instruction and its destination. Targets are not normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Call {
    pub site: InstructionSite,
    pub target: Address,
}

pub trait HasAddress {
    fn address(&self) -> u64;
}

impl HasAddress for u64 {
    fn address(&self) -> u64 {
        *self
    }
}

impl HasAddress for Address {
    fn address(&self) -> u64 {
        self.0
    }
}

impl HasAddress for InstructionSite {
    fn address(&self) -> u64 {
        self.0
    }
}

impl HasAddress for FunctionEntry {
    fn address(&self) -> u64 {
        self.start
    }
}

impl<'scope> BinaryMatcher<'scope> {
    pub fn pattern(pattern: Pattern) -> Self {
        Self::scan_patterns(vec![pattern])
    }

    /// Parse strings or accept existing patterns, failing immediately on invalid signatures.
    /// Accepts owned/borrowed Pattern and String values, &str, and borrowed &str collections.
    /// Submit scans together on resolution, retaining pattern order and duplicates.
    /// Errors identify the failing pattern's zero-based index; empty collections are valid.
    ///
    /// ```
    /// use patternsleuth::{binary_matchers::BinaryMatcher, resolvers::{Result, unreal::util}};
    /// # fn example() -> Result<()> {
    /// let matcher = BinaryMatcher::patterns(["48 89 ?? ??", "E8 | ?? ?? ?? ??"])?;
    /// let signatures = ["AA BB CC", "AA ?? CC"];
    /// let borrowed = BinaryMatcher::patterns(&signatures)?;
    /// let parsed = [util::utf16_pattern("foo\0")];
    /// let from_patterns = BinaryMatcher::patterns(&parsed)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn patterns<P: IntoPattern>(patterns: impl IntoIterator<Item = P>) -> Result<Self> {
        let patterns = patterns
            .into_iter()
            .enumerate()
            .map(|(index, pattern)| {
                pattern.into_pattern().map_err(|error| {
                    ResolveError::new_msg(format!("invalid pattern at index {index}: {error}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::scan_patterns(patterns))
    }

    fn scan_patterns(patterns: Vec<Pattern>) -> Self {
        let patterns = Arc::new(patterns);
        Self::from_async(move |(), ctx| {
            let patterns = Arc::clone(&patterns);
            Box::pin(async move {
                let results =
                    join_all(patterns.iter().map(|pattern| ctx.scan(pattern.clone()))).await;
                Ok(results.into_iter().flatten().collect())
            })
        })
    }

    /// Return an address without checking image bounds.
    pub fn address(address: u64) -> Self {
        Self::from_sync(move |()| Ok(vec![address]))
    }

    /// Build and evaluate another source matcher from the entire result vector.
    /// Runs even on empty results; inner errors skip the callback.
    pub fn sub<F>(self, func: F) -> Self
    where
        F: Fn(Vec<u64>) -> Result<BinaryMatcher<'scope>> + Send + Sync + 'scope,
    {
        self.with_context()
            .sub(move |(_, addresses)| func(addresses))
    }
}

impl<'scope, I: Send + 'scope, T: HasAddress + Send + 'scope> Matcher<'scope, I, Vec<T>> {
    /// Extract raw addresses, preserving order and duplicates.
    pub fn addresses(self) -> Matcher<'scope, I, Vec<u64>> {
        self.map_each(|node| node.address())
    }

    /// Find typed data-reference instruction sites using the existing scan heuristics.
    pub fn references(self) -> Matcher<'scope, I, Vec<InstructionSite>> {
        self.then(stages::references())
    }

    /// Find incoming direct call/jump instruction addresses, not function entries.
    pub fn callers(self) -> Matcher<'scope, I, Vec<InstructionSite>> {
        self.then(stages::callers())
    }

    /// Map to root-function metadata; skip unknown functions and propagate errors.
    pub fn root_functions(self) -> Matcher<'scope, I, Vec<FunctionEntry>> {
        self.then(stages::root_functions())
    }

    /// Inspect outgoing calls once, retaining both instruction sites and targets.
    /// Interior inputs start at their containing root entry; unknown functions are skipped.
    pub fn calls(self) -> Matcher<'scope, I, Vec<Call>> {
        self.then(stages::calls())
    }

    /// Return outgoing direct near-call targets; exclude indirect calls and tail jumps.
    pub fn callees(self) -> Matcher<'scope, I, Vec<Address>> {
        self.calls().map_each(|call| call.target)
    }

    pub fn call_sites(self) -> Matcher<'scope, I, Vec<InstructionSite>> {
        self.calls().map_each(|call| call.site)
    }

    /// Resolve inputs once, concatenate references then callers, and map to root entries.
    pub fn xrefs(self) -> Matcher<'scope, I, Vec<FunctionEntry>> {
        self.addresses()
            .then(stages::references::<u64>().union(stages::callers()))
            .then(stages::root_functions())
    }
}

impl<'scope, I: Send + 'scope> Matcher<'scope, I, Vec<u64>> {
    /// Add a signed offset, failing rather than wrapping on overflow.
    pub fn offset(self, offset: i64) -> Self {
        self.modify_with(move |address| {
            address.checked_add_signed(offset).ok_or_else(|| {
                ResolveError::new_msg(format!("address offset overflow: {address:#x} + {offset}"))
            })
        })
    }

    /// Transform each result into an absolute address, stopping at the first error.
    pub fn modify_with<F>(self, func: F) -> Self
    where
        F: Fn(u64) -> Result<u64> + Send + Sync + 'scope,
    {
        self.try_map_each(func)
    }
}

/// Typed stages for composing address, instruction, function, and call results.
pub mod stages {
    use super::*;

    /// Data-reference sites, including references via stored pointers; no metadata required.
    pub fn references<'scope, T: HasAddress + Send + 'scope>()
    -> Matcher<'scope, Vec<T>, Vec<InstructionSite>> {
        Matcher::new::<Vec<T>>()
            .map_each(|address| address.address())
            .then(Matcher::from_async(|addresses: Vec<u64>, ctx| {
                Box::pin(async move {
                    Ok(util::scan_xrefs(ctx, &addresses)
                        .await
                        .into_iter()
                        .map(InstructionSite)
                        .collect())
                })
            }))
    }

    /// Incoming direct call/jump sites, including targets via stored pointers.
    pub fn callers<'scope, T: HasAddress + Send + 'scope>()
    -> Matcher<'scope, Vec<T>, Vec<InstructionSite>> {
        Matcher::new::<Vec<T>>()
            .map_each(|address| address.address())
            .then(Matcher::from_async(|addresses: Vec<u64>, ctx| {
                Box::pin(async move {
                    Ok(util::scan_xcalls(ctx, &addresses)
                        .await
                        .into_iter()
                        .map(InstructionSite)
                        .collect())
                })
            }))
    }

    /// Map addresses to root-function metadata; skip unknown functions, retaining duplicates.
    ///
    /// ```
    /// use patternsleuth::binary_matchers::BinaryMatcher;
    /// let matcher = BinaryMatcher::address(0x1000)
    ///     .root_functions()
    ///     .filter(|function| function.size() >= 64)
    ///     .map_each(|function| function.start);
    /// ```
    pub fn root_functions<'scope, T: HasAddress + Send + 'scope>()
    -> Matcher<'scope, Vec<T>, Vec<FunctionEntry>> {
        Matcher::new::<Vec<T>>()
            .with_context()
            .try_map(|(ctx, input)| {
                let mut functions = Vec::new();
                for node in input {
                    if let Some(function) = ctx.image().get_root_function(node.address())? {
                        if function.range.end < function.range.start {
                            return Err(ResolveError::new_msg(format!(
                                "invalid function range: {:#x}..{:#x}",
                                function.range.start, function.range.end
                            )));
                        }
                        functions.push(FunctionEntry {
                            start: function.range.start,
                            end: function.range.end,
                            unwind: (function.unwind != 0).then_some(function.unwind),
                        });
                    }
                }
                Ok(functions)
            })
    }

    /// Reachable direct calls from each containing root, ordered by input then call-site IP.
    /// Excludes indirect calls and tail jumps; memory, metadata, and decode errors propagate.
    pub fn calls<'scope, T: HasAddress + Send + 'scope>() -> Matcher<'scope, Vec<T>, Vec<Call>> {
        Matcher::new::<Vec<T>>()
            .with_context()
            .try_map(|(ctx, input)| {
                let mut calls = Vec::new();
                for address in input {
                    calls.extend(function_calls(ctx, address.address())?);
                }
                Ok(calls)
            })
    }
}

/// Visit reachable blocks without following branches outside the containing root.
fn function_calls(ctx: &AsyncContext<'_>, address: u64) -> Result<Vec<Call>> {
    use crate::MemoryTrait;

    let image = ctx.image();
    let Some(root) = image.get_root_function(address)? else {
        return Ok(Vec::new());
    };
    let entry = root.range.start;
    let mut pending = vec![entry];
    let mut visited = HashSet::new();
    let mut calls = Vec::new();
    while let Some(block) = pending.pop() {
        if visited.contains(&block)
            || image.get_root_function(block)?.map(|f| f.range.start) != Some(entry)
        {
            continue;
        }
        let mut decoder = Decoder::with_ip(
            64,
            image.memory.range_from(block..)?,
            block,
            DecoderOptions::NONE,
        );
        while decoder.can_decode() {
            let ip = decoder.ip();
            if image.get_root_function(ip)?.map(|f| f.range.start) != Some(entry)
                || !visited.insert(ip)
            {
                break;
            }
            let instruction = decoder.decode();
            if instruction.is_invalid() {
                return Err(ResolveError::new_msg(format!(
                    "invalid instruction at {ip:#x}"
                )));
            }
            match instruction.flow_control() {
                FlowControl::Call if instruction.is_call_near() => {
                    calls.push(Call {
                        site: InstructionSite(ip),
                        target: Address(instruction.near_branch_target()),
                    });
                }
                FlowControl::ConditionalBranch => pending.push(instruction.near_branch_target()),
                FlowControl::UnconditionalBranch => {
                    pending.push(instruction.near_branch_target());
                    break;
                }
                FlowControl::Return | FlowControl::IndirectBranch | FlowControl::Exception => break,
                _ => {}
            }
        }
    }
    calls.sort_unstable_by_key(|call| call.site.0);
    Ok(calls)
}
