# patternsleuth
A test suite for finding robust patterns used to locate common functions and globals in Unreal Engine games. For use with [UE4SS](https://github.com/UE4SS-RE/RE-UE4SS).

## usage
Drop the game executables into the `games` directory:

```bash
$ tree games
games
├── AstroColony
│   └── AstroColony-Win64-Shipping.exe
├── FSD
│   └── FSD-Win64-Shipping.exe
└── TwinStick
    └── TwinStick-Win64-Shipping.exe
```

Run tests

```bash
$ cargo run --release
```

![tests](https://github.com/trumank/patternsleuth/assets/1144160/0591093c-ea8d-4201-998c-8c6eb4a7fdff)

## binary matcher tests

Run the synthetic-image tests without game executables:

```bash
cargo test -p patternsleuth --features image-pe --lib
cargo test -p patternsleuth --features image-elf --lib
cargo test -p patternsleuth --all-features --lib
cargo test -p patternsleuth_scanner --lib
```

The identical matcher cases in `patternsleuth/src/binary_matchers/tests/shared.rs`
run against PE and ELF; only the `construct_image` function differs. Constructors
in `patternsleuth/src/binary_matchers/tests/fixtures.rs` own all format-specific
function metadata.
ELF tests also parse a generated x86-64 ELF file and its mapped-image form,
discover functions from unwind records, and resolve nested string/xref pipelines.
Fixtures are generated in memory; no compiler or game binaries are required.
The xref patterns are x86-64-specific, not architecture-independent ELF support.

`patternsleuth::binary_matchers` provides composable matchers evaluated through
`patternsleuth::resolvers::eval`. Addresses are `u64`; signed offsets fail on
overflow. Pattern results retain order and duplicates. `First` and `Last` select
by result order, not address order. `XRefs` returns containing root functions,
not reference instruction addresses.

Construct matchers without explicit boxing:

`Pattern::from_bytes()` takes `NonEmptyBytes` and returns `Pattern` directly.
Use `neb![0x48, 0x8b, 0x01]` for explicit bytes (empty lists do not compile),
`NonEmptyBytes::new(first, rest)` when a first byte is known, or
`NonEmptyBytes::try_from(bytes)?` to validate a dynamic `Vec<u8>` once.

```rust
use patternsleuth::scanner::{neb, Pattern};

let pattern = Pattern::from_bytes(neb![0x48, 0x8b, 0x01]);
```

```rust
use patternsleuth::binary_matchers::BinaryMatcher;
use patternsleuth::scanner::{NonEmptyBytes, Pattern};

let bytes = NonEmptyBytes::new(b'A', b"sset loaded\0".iter().copied());
let matcher = BinaryMatcher::pattern(Pattern::from_bytes(bytes))
    .xrefs() // String readers
    .xrefs() // Their callers
    .first()
    .addresses()
    .offset(4)
    .sub(|addresses| {
        let address = addresses.first().copied().ok_or_else(|| {
            patternsleuth::resolvers::ResolveError::new_msg("no caller")
        })?;
        Ok(BinaryMatcher::address(address))
    });
```

Use `.with_context().modify_with()` for per-match transformations based on image memory:

```rust
use patternsleuth::MemoryTrait;

let matcher = matcher.with_context().modify_with(|(ctx, address)| {
    let offset = if ctx.image().memory.u8(address)? == 0x48 { 3 } else { 2 };
    address.checked_add(offset).ok_or_else(|| {
        patternsleuth::resolvers::ResolveError::new_msg("address overflow")
    })
});
```

The callback runs once per address in result order and returns an absolute
address, not an offset. Duplicates are retained; empty input skips the callback.
The first error stops resolution. `.modify_with(|address| ...)` is the data-only
version. Unlike per-item modifiers, `.sub(|addresses| ...)` runs once with the
full result vector and returns another matcher; use `.with_context().sub()` when
building that matcher requires image access.

Use `.filter()` to keep addresses matching a predicate, or `.filter_modify()`
to transform and filter in one callback:

```rust
let matcher = matcher
    .filter(|address| address != 0)
    .with_context()
    .filter_modify(|(ctx, address)| {
        if ctx.image().memory.u8(address)? == 0x48 {
            Ok(Some(address)) // Keep this absolute address.
        } else {
            Ok(None) // Drop this match.
        }
    });
```

Both preserve order and duplicates and skip callbacks on empty input.
`filter` returns `bool`; `filter_modify` returns `Result<Option<u64>>` and stops
at the first error, with or without context.

### Reference and call traversal

`BinaryMatcher::patterns()` accepts signature strings or parsed patterns, including
owned and borrowed collections and iterators. Signatures are parsed once at
construction; invalid input returns an error with its zero-based pattern index.
Scans are submitted together when resolved, preserving pattern order and duplicates.

```rust
use patternsleuth::{binary_matchers::BinaryMatcher, resolvers::unreal::util};

let matcher = BinaryMatcher::patterns([
    "48 89 ?? ??",
    "E8 | ?? ?? ?? ??",
])?
.references()
.root_functions();

let signatures = ["AA BB CC", "AA ?? CC"];
let matcher = BinaryMatcher::patterns(&signatures)?;

let matcher = BinaryMatcher::patterns([
    util::utf16_pattern("foo\0"),
    util::utf16_pattern("bar\0"),
])?;
```

Add `?` to existing `patterns()` calls, even when inputs are already parsed.
`pattern(Pattern)` remains infallible. Empty collections are valid; empty signature
strings are not. No manual `.map(|p| Pattern::new(p).unwrap())` is needed.

`.root_functions()` returns `FunctionEntry` values with `start`, exclusive
`end`, `size()`, and optional `unwind` addresses. Size describes the root's recorded
range, not separate chained fragments. Argument types and counts are not inferred.
Traversal methods retain typed nodes and accept any node implementing `HasAddress`,
so they chain directly. Use `.addresses()` to extract raw `u64` values before
address arithmetic, address-only callbacks, or resolver helpers expecting integers.

```rust
let functions = BinaryMatcher::patterns(["AA BB CC"])?
    .root_functions()
    .filter(|function| function.size() >= 64)
    .resolve(ctx)
    .await?;
```

Matchers are typed stages: `Matcher<'scope, Input, Output>`. `BinaryMatcher<'scope>`
is the address-producing source alias `Matcher<'scope, (), Vec<u64>>`; existing
fluent methods remain available. It is no longer an enum: direct variant construction or
pattern matching must use the fluent API instead. Stages share read-only image
context, not mutable state.

Inputs, outputs, and transformation captures may borrow for `'scope`; they need
not be `'static`. The lifetime is usually inferred:

```rust
let prefix = String::from("prefix:");
let stage = Matcher::new::<&str>()
    .map(|value| value.strip_prefix(prefix.as_str()).unwrap_or(value));
let result = stage.run("prefix:payload", ctx).await?;
```

Each evaluation borrows its context independently. `.with_context()` callbacks
cannot return references into that context. The low-level `from_async`
callback must support every context lifetime; use `new` and the
transformation methods for borrowed pipeline data.

`Matcher::new::<Input>()` starts a pass-through stage.
`Matcher::from_sync(callback)` starts a stage from a data-only callback returning
`resolvers::Result<Output>`. `Matcher::from_async(callback)` accepts input and
context and returns a boxed future yielding the same result type.

All fallible matcher APIs, constructors, and callbacks use
`patternsleuth::resolvers::Result<T>` (`Result<T, ResolveError>`), matching UE
resolvers. Memory errors retain their native error variant. Pattern-parser
errors become resolver messages with the pattern index and parser diagnostics.
No `matcher_error` adapter is needed:

```rust
use patternsleuth::resolvers::{ensure_one, Result};

let matcher = BinaryMatcher::patterns(["AA BB CC", "AA DD CC"])?
    .try_map(ensure_one);
let address: Result<u64> = matcher.resolve(ctx).await;
```

Callbacks returning `anyhow::Error` must now construct or explicitly convert to
`ResolveError`; use `ResolveError::new_msg(...)` for custom failures. Memory reads
continue to work with `?`.

Transformation callbacks receive only their data. Context belongs to each run,
not to the matcher. `.with_context()` supplies `(ctx, value)` to the next
transformation only, then returns an ordinary matcher:

```rust
let targets = matcher.calls().map_each(|call| call.target);
let count = targets.map(|targets| targets.len());

let bytes = BinaryMatcher::address(address)
    .with_context()
    .try_map_each(|(ctx, address)| Ok(ctx.image().memory.u8(address)?))
    .map_each(u64::from);
```

The adapter supports `map`, `try_map`, `map_each`, `try_map_each`, `flat_map`,
`try_flat_map`, `filter`, `filter_each`, `try_filter`, `filter_modify`,
`modify_with`, and `sub`. Whole-state operations receive `(ctx, output)`;
per-item operations receive `(ctx, item)`, with borrowed items for `filter_each`
and `try_filter`. Callbacks are synchronous, and context cannot escape in their
output. Use another `.with_context()` whenever a later transformation needs it.

The former `*_with_context` methods have been removed. For example, replace
`.try_map_each_with_context(|value, ctx| ...)` with
`.with_context().try_map_each(|(ctx, value)| ...)`. Built-in traversal stages
access context internally. A matcher can be reused with different images.

- `.then(stage)` passes the entire output to the next stage.
- `.map()` / `.try_map()` transform the entire output state.
- `.map_each()` / `.try_map_each()` transform individual vector items.
- `.flat_map()` / `.try_flat_map()` expand individual items.
- `.filter()` takes copyable items by value; `.filter_each()` and
  `.try_filter()` borrow items, including non-`Copy` values.
- `.filter_modify()` transforms or drops individual items.
- `.fork(other)` runs both stages on the same input, left first, returning a
  tuple. `.concat()` concatenates paired vector outputs.
- `.union(other)` concatenates branch outputs; `.intersection(other)` keeps
  left items found in the right output, preserving left order and duplicates.

Put branching **after** a shared upstream stage to evaluate that stage once:
`source.then(left.fork(right).concat())`. Fork clones its input once; cloning a
matcher shares its implementation but does not cache results. Branches stop at
the first error. No combinator implicitly sorts or deduplicates results.

Typed traversal stages distinguish `Address`, `InstructionSite`, `FunctionEntry`,
and `Call { site, target }`. For example:

```rust
use patternsleuth::{
    binary_matchers::{BinaryMatcher, Call, Matcher},
    resolvers::unreal::util,
};

let calls: Matcher<'_, (), Vec<Call>> =
    BinaryMatcher::pattern(util::utf16_pattern("foo\0"))
        .references()     // Vec<InstructionSite>
        .root_functions() // Vec<FunctionEntry>
        .callers()        // Vec<InstructionSite>
        .root_functions() // Vec<FunctionEntry>
        .calls();         // Vec<Call>, with both sites and targets

let calls = calls.resolve(ctx).await?;
```

Use `.run(input, ctx).await` for stages with explicit input, or `.resolve(ctx)`
for sources taking `()`. `Matcher::from_async()` accepts custom asynchronous stages
returning boxed futures. Adjacent stages must have compatible output/input types.

| Method | Returned nodes |
| --- | --- |
| `.references()` | `InstructionSite`: data-reference sites pointing to the inputs |
| `.callers()` | `InstructionSite`: incoming direct call/jump sites targeting the inputs |
| `.root_functions()` | `FunctionEntry`: containing root-function metadata |
| `.callees()` | `Address`: outgoing direct near-call targets |
| `.call_sites()` | `InstructionSite`: outgoing direct near-call sites |
| `.xrefs()` | `FunctionEntry`: roots containing data references and incoming calls/jumps |

`references()` and `callers()` retain the existing x86-64 scan heuristics,
including references via stored address pointers. They do not require function
metadata. `xrefs()` is a composition of references, callers, concatenation, and
root-function lookup; it evaluates its upstream matcher once and retains its
existing behavior and result ordering. `.calls()` returns both outgoing call
sites and targets from a single traversal.

Load function metadata with `Image::builder().functions(true)` for function
operations. `root_functions()`, `callees()`, and `call_sites()` skip addresses
outside known functions and propagate metadata errors. Outgoing calls are
discovered from the containing function's entry, even for interior inputs.
Traversal follows reachable local branches but not calls into other functions;
indirect calls and tail jumps are excluded. Memory and invalid-instruction
errors propagate. Results retain duplicates, in input order then call-site
address order; targets are not normalized or followed through jump stubs.

```rust
use patternsleuth::resolvers::unreal::util;

let matcher = BinaryMatcher::pattern(util::utf16_pattern("foo\0"))
    .references()     // Instructions referencing the string.
    .root_functions() // Functions containing those instructions.
    .callers()        // Instructions calling/jumping to those functions.
    .root_functions() // Functions containing those incoming call/jump sites.
    .callees();       // Direct call targets from those functions.
```

## functions referencing two strings

Run `patternsleuth/examples/two_strings.rs` against a PE or x86-64 ELF binary:

```bash
cargo run -p patternsleuth --example two_strings --features image-pe,image-elf -- \
  /path/to/game "First string" "Second string"
```

The example resolves the functions referencing each null-terminated UTF-8
literal, then intersects the results. It prints each common function start once.
It finds the second literal's references across the image, not via a
function-scoped scan. References follow the existing x86-64 xref heuristics and
require function metadata; this does not cover every possible runtime string use.

## acknowledgements
Thanks to,
- [LongerWarrior](https://github.com/LongerWarrior) - for providing a truly massive collection of games to test against as well as finding many very reliable patterns and providing lots of assistance with reversing of more unusual games
- [Narknon](https://github.com/Narknon) - for providing many games as well as clean source builds with symbols
- [FransBouma](https://github.com/FransBouma) - for seeding the project with [UUU](https://opm.fransbouma.com/uuuv5.htm) patterns and many game dumps
- [praydog](https://github.com/praydog) - for inspiration and ideas behind string based symbol resolvers, similar to those used in [UEVR](https://github.com/praydog/UEVR)
