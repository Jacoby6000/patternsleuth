use super::*;
use crate::{
    NamedMemorySection,
    image::Image,
    resolvers::{Context as _, bail_out, eval},
};
use object::SectionKind;

#[allow(dead_code)]
mod two_strings_example {
    use crate as patternsleuth;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/two_strings.rs"
    ));
}

fn section(
    name: &str,
    address: u64,
    kind: SectionKind,
    data: Vec<u8>,
) -> NamedMemorySection<'static> {
    NamedMemorySection::new(name.into(), address, kind, data)
}

fn image(text: Vec<u8>) -> Image<'static> {
    construct_image(text, 2)
}

fn run<T: HasAddress + Send>(
    image: &Image<'_>,
    matcher: Matcher<'_, (), Vec<T>>,
) -> Result<Vec<u64>> {
    eval(image, move |ctx| {
        Box::pin(async move { matcher.addresses().resolve(ctx).await })
    })
}

fn patterns<'scope>(values: &[&str]) -> BinaryMatcher<'scope> {
    BinaryMatcher::patterns(values).unwrap()
}

fn run_stage<I, O>(image: &Image<'_>, matcher: Matcher<I, O>, input: I) -> Result<O>
where
    I: Send + Sync,
    O: Send + Sync,
{
    eval(image, move |ctx| {
        Box::pin(async move { matcher.run(input, ctx).await })
    })
}

#[test]
fn matchers_compose_directly_with_native_resolver_results() {
    fn build<'scope>() -> crate::resolvers::Result<Matcher<'scope, (), u64>> {
        Ok(
            BinaryMatcher::patterns(["AA DD CC", "AA DD CC"])?
                .try_map(crate::resolvers::ensure_one),
        )
    }
    let result: crate::resolvers::Result<u64> = run_stage(&scan_image(), build().unwrap(), ());
    assert_eq!(result.unwrap(), TEXT + 24);

    let ambiguous = BinaryMatcher::patterns(["AA BB CC"])
        .unwrap()
        .try_map(crate::resolvers::ensure_one);
    assert_eq!(
        run_stage(&scan_image(), ambiguous, ()).unwrap_err(),
        crate::resolvers::ensure_one([TEXT + 8, TARGET]).unwrap_err()
    );
}

#[test]
fn matcher_callbacks_preserve_native_error_variants_without_conversion() {
    fn reject(_: Vec<u64>) -> crate::resolvers::Result<Vec<u64>> {
        Err(crate::MemoryAccessError::MisalginedAddress(TEXT, TEXT + 8).into())
    }

    let matcher = BinaryMatcher::address(TEXT)
        .try_map(reject)
        .first()
        .xrefs()
        .addresses()
        .with_context()
        .sub(|_| panic!("native error must skip subsequent callbacks"));
    assert_eq!(
        run(&scan_image(), matcher).unwrap_err(),
        ResolveError::from(crate::MemoryAccessError::MisalginedAddress(TEXT, TEXT + 8))
    );
}

#[test]
fn new_passes_non_clone_input_and_infers_transformed_output() {
    struct Input {
        addresses: Vec<u64>,
    }

    let stage = Matcher::new::<Input>()
        .map(|input| input.addresses)
        .map(|addresses| addresses.len());
    assert_eq!(
        run_stage(
            &scan_image(),
            stage,
            Input {
                addresses: vec![TEXT, TARGET]
            }
        )
        .unwrap(),
        2
    );

    let stage = Matcher::new::<Vec<u64>>().map_each(|address| address + 1);
    let matcher = BinaryMatcher::address(TEXT).then(stage);
    assert_eq!(run(&scan_image(), matcher).unwrap(), [TEXT + 1]);
}

#[test]
fn from_sync_infers_types_and_accepts_borrowed_inputs_outputs_and_captures() {
    let text = String::from("prefix:payload");
    let prefix = String::from("prefix:");
    let stage =
        Matcher::from_sync(|value: &str| Ok(value.strip_prefix(prefix.as_str()).unwrap_or(value)));
    assert_eq!(
        run_stage(&scan_image(), stage, text.as_str()).unwrap(),
        "payload"
    );

    struct Input(Vec<u64>);
    let stage = Matcher::from_sync(|input: Input| Ok(input.0.len()));
    assert_eq!(
        run_stage(&scan_image(), stage, Input(vec![TEXT])).unwrap(),
        1
    );
}

#[test]
fn from_sync_preserves_native_errors_and_skips_subsequent_stages() {
    let stage = Matcher::from_sync(|_: Vec<u64>| -> Result<Vec<u64>> {
        Err(crate::MemoryAccessError::MemoryOutOfBoundsError.into())
    });
    let matcher = BinaryMatcher::address(TEXT)
        .then(stage)
        .map(|_| panic!("callback must not run after an error"));
    assert_eq!(
        run_stage(&scan_image(), matcher, ()).unwrap_err(),
        ResolveError::from(crate::MemoryAccessError::MemoryOutOfBoundsError)
    );
}

#[test]
fn stages_accept_borrowed_inputs_outputs_and_captures() {
    let text = String::from("prefix:payload");
    let prefix = String::from("prefix:");
    let stage = Matcher::new::<&str>().map(|value| value.strip_prefix(prefix.as_str()).unwrap());
    let result = run_stage(&scan_image(), stage, text.as_str()).unwrap();
    assert_eq!(result, "payload");
    assert_eq!(result.as_ptr(), text[prefix.len()..].as_ptr());
}

#[test]
fn borrowed_stages_compose_with_context_branches_and_sub() {
    use crate::MemoryTrait;

    let values = vec![TEXT + 8, TEXT + 24, TEXT + 8];
    let threshold = TEXT + 8;
    let expected = String::from("AA");
    let stage = Matcher::new::<&[u64]>()
        .map(|values| values.to_vec())
        .with_context()
        .filter_modify(|(ctx, address)| {
            assert_eq!(format!("{:02X}", ctx.image().memory.u8(address)?), expected);
            Ok((address >= threshold).then_some(address))
        });
    let left = stage.clone().first();
    let right = stage.last();
    assert_eq!(
        run_stage(&scan_image(), left.union(right), values.as_slice()).unwrap(),
        [TEXT + 8, TEXT + 8]
    );

    let matcher = BinaryMatcher::address(TEXT).sub(|_| {
        Ok(BinaryMatcher::address(TEXT).map(|_| values.as_slice()))
            .map(|stage| stage.map(|addresses| addresses.to_vec()))
    });
    assert_eq!(run(&scan_image(), matcher).unwrap(), values);
}

#[test]
fn borrowed_outputs_survive_a_shorter_evaluation_context() {
    let text = String::from("borrowed result");
    let result = {
        let image = scan_image();
        let matcher = BinaryMatcher::address(TEXT).map(|_| text.as_str());
        eval(&image, move |ctx| {
            Box::pin(async move { matcher.resolve(ctx).await })
        })
        .unwrap()
    };
    assert_eq!(result, text);
}

#[test]
fn typed_stages_accept_arbitrary_non_clone_state_and_change_output_types() {
    struct State {
        name: String,
        address: u64,
    }

    let matcher = Matcher::identity()
        .map(|state: State| (state.name, state.address))
        .then(Matcher::from_async(
            |(name, address): (String, u64), ctx| {
                Box::pin(async move {
                    assert!(!ctx.image().memory.sections.is_empty());
                    Ok(format!("{name}@{address:x}"))
                })
            },
        ));
    assert_eq!(
        run_stage(
            &scan_image(),
            matcher,
            State {
                name: "foo".into(),
                address: 42
            }
        )
        .unwrap(),
        "foo@2a"
    );
}

#[test]
fn fork_evaluates_shared_upstream_once_and_branches_left_then_right() {
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream_seen = Arc::clone(&seen);
    let left_seen = Arc::clone(&seen);
    let right_seen = Arc::clone(&seen);
    let matcher = BinaryMatcher::address(42)
        .map(move |items| {
            upstream_seen.lock().unwrap().push("source");
            items
        })
        .then(
            Matcher::identity()
                .map(move |items: Vec<u64>| {
                    left_seen.lock().unwrap().push("left");
                    items
                })
                .fork(Matcher::identity().map(move |mut items: Vec<u64>| {
                    right_seen.lock().unwrap().push("right");
                    items.push(7);
                    items
                }))
                .concat(),
        );
    let image = scan_image();
    eval(&image, move |ctx| {
        Box::pin(async move {
            for _ in 0..2 {
                assert_eq!(matcher.resolve(ctx).await.unwrap(), [42, 42, 7]);
            }
        })
    });
    assert_eq!(
        *seen.lock().unwrap(),
        ["source", "left", "right", "source", "left", "right"]
    );
}

#[test]
fn branch_and_upstream_errors_skip_later_stages() {
    let left: Matcher<Vec<u64>, Vec<u64>> =
        Matcher::identity().try_map(|_| Err(ResolveError::new_msg("left failed")));
    let right: Matcher<Vec<u64>, Vec<u64>> =
        Matcher::identity().map(|_| panic!("right must not run after left error"));
    assert_eq!(
        run_stage(&scan_image(), left.fork(right).concat(), vec![1])
            .unwrap_err()
            .to_string(),
        "left failed"
    );
    let source = BinaryMatcher::address(0).offset(-1);
    let branch: Matcher<Vec<u64>, Vec<u64>> =
        Matcher::identity().map(|_| panic!("branches must not run after upstream error"));
    assert!(
        run(
            &scan_image(),
            source.then(branch.clone().fork(branch).concat())
        )
        .is_err()
    );

    let left: Matcher<Vec<u64>, Vec<u64>> = Matcher::identity();
    let right: Matcher<Vec<u64>, Vec<u64>> =
        Matcher::identity().try_map(|_| Err(ResolveError::new_msg("right failed")));
    assert_eq!(
        run_stage(&scan_image(), left.fork(right).concat(), vec![1])
            .unwrap_err()
            .to_string(),
        "right failed"
    );
}

#[test]
fn union_and_intersection_preserve_left_order_and_multiplicity() {
    let left: Matcher<Vec<u64>, Vec<u64>> = Matcher::identity();
    let right = Matcher::identity().map(|_: Vec<u64>| vec![3, 2]);
    assert_eq!(
        run_stage(
            &scan_image(),
            left.clone().union(right.clone()),
            vec![3, 1, 3, 2]
        )
        .unwrap(),
        [3, 1, 3, 2, 3, 2]
    );
    assert_eq!(
        run_stage(&scan_image(), left.intersection(right), vec![3, 1, 3, 2]).unwrap(),
        [3, 3, 2]
    );
}

#[test]
fn collection_stages_handle_owned_non_copy_items_and_expansion() {
    let matcher = Matcher::identity()
        .map_each(|word: String| word.to_uppercase())
        .filter_each(|word| word != "DROP")
        .try_filter(|word| Ok(!word.is_empty()))
        .flat_map(|word| vec![word.clone(), word])
        .filter_modify(|word| Ok(Some(word.len())))
        .try_map_each(|len| Ok(len + 1));
    assert_eq!(
        run_stage(
            &scan_image(),
            matcher,
            vec!["foo".into(), "drop".into(), "".into()]
        )
        .unwrap(),
        [4, 4]
    );
}

#[test]
fn explicit_context_combinators_support_whole_states_and_individual_items() {
    use crate::MemoryTrait;

    let matcher = Matcher::<Vec<u64>, Vec<u64>>::identity()
        .with_context()
        .map(|(ctx, addresses)| {
            assert!(!ctx.image().memory.sections.is_empty());
            addresses
        })
        .with_context()
        .try_map(|(ctx, addresses)| {
            assert_eq!(ctx.image().memory.u8(addresses[0])?, 0xaa);
            Ok(addresses)
        })
        .with_context()
        .map_each(|(ctx, address)| {
            assert!(!ctx.image().memory.sections.is_empty());
            address
        })
        .with_context()
        .try_map_each(|(ctx, address)| {
            assert_eq!(ctx.image().memory.u8(address)?, 0xaa);
            Ok(address)
        })
        .with_context()
        .flat_map(|(ctx, address)| {
            assert!(!ctx.image().memory.sections.is_empty());
            vec![address, address]
        })
        .with_context()
        .try_flat_map(|(ctx, address)| {
            assert_eq!(ctx.image().memory.u8(address)?, 0xaa);
            Ok(vec![address])
        })
        .with_context()
        .filter_each(|(ctx, address)| ctx.image().memory.u8(*address).unwrap() == 0xaa)
        .with_context()
        .try_filter(|(ctx, address)| Ok(ctx.image().memory.u8(*address)? == 0xaa))
        .with_context()
        .filter(|(ctx, address)| ctx.image().memory.u8(address + 1).unwrap() == 0xbb)
        .with_context()
        .filter_modify(|(ctx, address)| {
            Ok((ctx.image().memory.u8(address + 1)? == 0xbb).then_some(address + 1))
        });
    assert_eq!(
        run_stage(&scan_image(), matcher, vec![TEXT + 8, TEXT + 24, TEXT + 8]).unwrap(),
        [TEXT + 9; 4]
    );
}

#[test]
fn context_combinators_use_each_runs_image_instead_of_capturing_one() {
    use crate::MemoryTrait;

    let matcher = BinaryMatcher::address(TEXT + 8)
        .with_context()
        .try_map_each(|(ctx, address)| Ok(ctx.image().memory.u8(address)?));
    let first = scan_image();
    let mut second = scan_image();
    second.memory.sections[0].data.to_mut()[8] = 0xbb;
    assert_eq!(run_stage(&first, matcher.clone(), ()).unwrap(), [0xaa]);
    assert_eq!(run_stage(&second, matcher, ()).unwrap(), [0xbb]);
}

#[test]
fn context_collection_callbacks_skip_empty_input_and_propagate_errors() {
    let stage: Matcher<Vec<u64>, Vec<u64>> = Matcher::identity()
        .with_context()
        .try_map_each(|(_, _)| -> Result<u64> { panic!("empty map") })
        .with_context()
        .flat_map(|(_, _)| -> Vec<u64> { panic!("empty flat map") })
        .with_context()
        .try_flat_map(|(_, _)| -> Result<Vec<u64>> { panic!("empty try flat map") })
        .with_context()
        .filter_each(|(_, _)| panic!("empty filter"))
        .with_context()
        .try_filter(|(_, _)| panic!("empty try filter"))
        .with_context()
        .filter_modify(|(_, _)| panic!("empty filter modify"));
    assert!(run_stage(&scan_image(), stage, vec![]).unwrap().is_empty());

    for operation in [0, 1, 2] {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let check = move |item: u64, ctx: &AsyncContext<'_>| {
            assert!(!ctx.image().memory.sections.is_empty());
            captured.lock().unwrap().push(item);
            if item == 2 {
                bail_out!("context item failed");
            }
            Ok(item)
        };
        let stage = match operation {
            0 => Matcher::identity()
                .with_context()
                .try_map_each(move |(ctx, item)| check(item, ctx)),
            1 => Matcher::identity()
                .with_context()
                .try_flat_map(move |(ctx, item)| Ok(vec![check(item, ctx)?])),
            _ => Matcher::identity()
                .with_context()
                .try_filter(move |(ctx, item)| Ok(check(*item, ctx)? != 0)),
        };
        assert_eq!(
            run_stage(&scan_image(), stage, vec![1, 2, 3])
                .unwrap_err()
                .to_string(),
            "context item failed"
        );
        assert_eq!(*seen.lock().unwrap(), [1, 2]);
    }
}

#[test]
fn context_adapter_is_lazy_scoped_and_evaluates_upstream_once() {
    use crate::MemoryTrait;
    use std::sync::{Arc, Mutex};

    let count = Arc::new(Mutex::new(0));
    let captured = Arc::clone(&count);
    let matcher = BinaryMatcher::address(TEXT + 8)
        .map_each(move |address| {
            *captured.lock().unwrap() += 1;
            address
        })
        .with_context()
        .try_map_each(|(ctx, address)| Ok(ctx.image().memory.u8(address)?))
        .map_each(u64::from)
        .with_context()
        .try_map(|(ctx, bytes)| {
            assert_eq!(bytes[0], u64::from(ctx.image().memory.u8(TEXT + 8)?));
            Ok(bytes)
        })
        .map(|bytes| bytes.len());
    assert_eq!(*count.lock().unwrap(), 0);
    assert_eq!(run_stage(&scan_image(), matcher, ()).unwrap(), 1);
    assert_eq!(*count.lock().unwrap(), 1);
}

#[test]
fn context_adapter_borrows_non_copy_items_without_requiring_clone() {
    use crate::MemoryTrait;

    struct Item {
        name: String,
    }
    let matcher = Matcher::<Vec<Item>, Vec<Item>>::identity()
        .with_context()
        .filter_each(|(ctx, item)| {
            ctx.image().memory.u8(TEXT + 8).unwrap() == 0xaa && !item.name.is_empty()
        })
        .with_context()
        .try_filter(|(ctx, item)| {
            Ok(ctx.image().memory.u8(TEXT + 8)? == 0xaa && item.name != "drop")
        })
        .with_context()
        .filter_modify(|(ctx, item)| Ok((ctx.image().memory.u8(TEXT + 8)? == 0xaa).then_some(item)))
        .map_each(|item| item.name);
    let input = ["foo", "", "drop", "bar"]
        .map(|name| Item { name: name.into() })
        .into();
    assert_eq!(
        run_stage(&scan_image(), matcher, input).unwrap(),
        ["foo", "bar"]
    );
}

#[test]
fn context_adapter_whole_state_callbacks_run_on_empty_input_but_not_upstream_errors() {
    let matcher = patterns(&[]).with_context().map(|(ctx, addresses)| {
        assert!(!ctx.image().memory.sections.is_empty());
        addresses.len()
    });
    assert_eq!(run_stage(&scan_image(), matcher, ()).unwrap(), 0);

    let failed = BinaryMatcher::address(0)
        .offset(-1)
        .with_context()
        .map(|_| -> Vec<u64> { panic!("upstream error must skip callback") })
        .with_context()
        .try_map_each(|_| -> Result<u64> { panic!("upstream error must skip item callback") });
    assert!(
        run(&scan_image(), failed)
            .unwrap_err()
            .to_string()
            .contains("address offset overflow")
    );
}

#[test]
fn context_adapter_filter_modify_stops_at_first_error_after_none() {
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let matcher: Matcher<Vec<u64>, Vec<u64>> =
        Matcher::identity()
            .with_context()
            .filter_modify(move |(ctx, item)| {
                assert!(!ctx.image().memory.sections.is_empty());
                captured.lock().unwrap().push(item);
                if item == 2 {
                    bail_out!("context filter failed");
                }
                Ok(None)
            });
    assert_eq!(
        run_stage(&scan_image(), matcher, vec![1, 2, 3])
            .unwrap_err()
            .to_string(),
        "context filter failed"
    );
    assert_eq!(*seen.lock().unwrap(), [1, 2]);
}

#[test]
fn typed_collection_callbacks_skip_empty_and_stop_on_first_error() {
    let empty: Matcher<Vec<String>, Vec<String>> = Matcher::identity()
        .try_map_each(|_: String| -> Result<String> { panic!("empty map") })
        .try_flat_map(|_: String| -> Result<Vec<String>> { panic!("empty flat map") })
        .try_filter(|_| panic!("empty filter"));
    assert!(run_stage(&scan_image(), empty, vec![]).unwrap().is_empty());

    for operation in [0, 1, 2] {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let check = move |item: u64| {
            captured.lock().unwrap().push(item);
            if item == 2 {
                bail_out!("item failed");
            }
            Ok(item)
        };
        let stage = match operation {
            0 => Matcher::identity().try_map_each(check),
            1 => Matcher::identity().try_flat_map(move |item| Ok(vec![check(item)?])),
            _ => Matcher::identity().try_filter(move |item| Ok(check(*item)? != 0)),
        };
        assert_eq!(
            run_stage(&scan_image(), stage, vec![1, 2, 3])
                .unwrap_err()
                .to_string(),
            "item failed"
        );
        assert_eq!(*seen.lock().unwrap(), [1, 2]);
    }
}

#[test]
fn typed_traversal_preserves_function_site_and_call_target_types() {
    let matcher = string_matcher(NARROW_TEXT, false)
        .map_each(Address)
        .then(stages::references())
        .then(stages::root_functions())
        .then(stages::callers())
        .then(stages::root_functions())
        .then(stages::calls());
    let calls = run_stage(&string_graph(), matcher, ()).unwrap();
    assert_eq!(
        calls,
        [
            Call {
                site: InstructionSite(SHARED_CALLER + 8),
                target: Address(READER_A)
            },
            Call {
                site: InstructionSite(SHARED_CALLER + 0x18),
                target: Address(READER_B)
            },
            Call {
                site: InstructionSite(OTHER_CALLER + 8),
                target: Address(READER_A)
            },
        ]
    );
}

#[test]
fn composed_xrefs_evaluates_callbacks_once_and_retains_legacy_order() {
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let matcher = BinaryMatcher::address(READER_A)
        .modify_with(move |address| {
            captured.lock().unwrap().push(address);
            Ok(address)
        })
        .xrefs();
    assert_eq!(
        run(&string_graph(), matcher).unwrap(),
        [SHARED_CALLER, OTHER_CALLER]
    );
    assert_eq!(*seen.lock().unwrap(), [READER_A]);
}

fn scan_image() -> Image<'static> {
    let mut text = vec![0x90; 256];
    text[8..11].copy_from_slice(&[0xaa, 0xbb, 0xcc]);
    text[24..27].copy_from_slice(&[0xaa, 0xdd, 0xcc]);
    let mut image = image(text);
    image.memory.sections.push(section(
        ".data",
        TARGET,
        SectionKind::Data,
        vec![0xaa, 0xbb, 0xcc],
    ));
    image
}

#[test]
fn patterns_accept_owned_and_borrowed_strings_patterns_and_iterators() {
    let signatures = ["AA BB CC"];
    let strings = vec![String::from("AA BB CC")];
    let parsed = vec![Pattern::new("AA BB CC").unwrap()];
    let matchers = [
        BinaryMatcher::patterns(signatures).unwrap(),
        BinaryMatcher::patterns(&signatures).unwrap(),
        BinaryMatcher::patterns(signatures.iter()).unwrap(),
        BinaryMatcher::patterns(&strings).unwrap(),
        BinaryMatcher::patterns(strings.iter()).unwrap(),
        BinaryMatcher::patterns(strings).unwrap(),
        BinaryMatcher::patterns(&parsed).unwrap(),
        BinaryMatcher::patterns(parsed.iter()).unwrap(),
        BinaryMatcher::patterns(parsed).unwrap(),
    ];
    for matcher in matchers {
        assert_eq!(run(&scan_image(), matcher).unwrap(), [TEXT + 8, TARGET]);
    }
}

#[test]
fn patterns_parse_borrowed_strings_without_retaining_their_lifetime() {
    let matcher = {
        let strings = vec![
            String::from("AA DD CC"),
            String::from("AA BB CC"),
            String::from("AA DD CC"),
        ];
        BinaryMatcher::patterns(&strings).unwrap()
    };
    assert_eq!(
        run(&scan_image(), matcher).unwrap(),
        [TEXT + 24, TEXT + 8, TARGET, TEXT + 24]
    );
}

#[test]
fn patterns_preserve_markers_in_strings_and_borrowed_parsed_patterns() {
    let parsed = [Pattern::new("AA | DD CC").unwrap()];
    for matcher in [
        BinaryMatcher::patterns(["AA | DD CC"]).unwrap(),
        BinaryMatcher::patterns(&parsed).unwrap(),
        BinaryMatcher::patterns(parsed).unwrap(),
    ] {
        assert_eq!(run(&scan_image(), matcher).unwrap(), [TEXT + 25]);
    }
}

#[test]
fn patterns_convert_inputs_once_at_construction_not_on_each_resolution() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CountedSignature(Arc<AtomicUsize>);
    impl IntoPattern for CountedSignature {
        fn into_pattern(self) -> Result<Pattern> {
            self.0.fetch_add(1, Ordering::SeqCst);
            "AA DD CC".into_pattern()
        }
    }

    let count = Arc::new(AtomicUsize::new(0));
    let matcher = BinaryMatcher::patterns([CountedSignature(Arc::clone(&count))]).unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let image = scan_image();
    eval(&image, move |ctx| {
        Box::pin(async move {
            for _ in 0..2 {
                assert_eq!(matcher.resolve(ctx).await.unwrap(), [TEXT + 24]);
            }
        })
    });
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn patterns_reject_invalid_signatures_at_construction_with_their_index() {
    let error = BinaryMatcher::patterns(["AA BB CC", "not-a-pattern", "AA DD CC"])
        .err()
        .expect("invalid signature must fail before resolution");
    assert!(error.to_string().starts_with("invalid pattern at index 1:"));
    assert!(format!("{error:#}").contains("bad pattern word"));

    let error = BinaryMatcher::patterns([""])
        .err()
        .expect("empty signature must fail");
    assert!(error.to_string().starts_with("invalid pattern at index 0:"));
    assert!(format!("{error:#}").contains("pattern must contain at least one byte"));
}

#[test]
fn flexible_patterns_accept_empty_collections() {
    for matcher in [
        BinaryMatcher::patterns(Vec::<String>::new()).unwrap(),
        BinaryMatcher::patterns(Vec::<Pattern>::new()).unwrap(),
        BinaryMatcher::patterns(std::iter::empty::<&str>()).unwrap(),
    ] {
        assert!(run(&scan_image(), matcher).unwrap().is_empty());
    }
}

#[test]
fn non_empty_bytes_macro_is_reexported_for_literal_patterns() {
    let matcher =
        BinaryMatcher::pattern(crate::scanner::Pattern::from_bytes(crate::scanner::neb![
            0xaa, 0xbb, 0xcc
        ]));
    assert_eq!(run(&scan_image(), matcher).unwrap(), [TEXT + 8, TARGET]);
}

#[test]
fn fluent_constructors_compose_with_owned_callbacks_and_offsets() {
    let expected = vec![TEXT + 24];
    let matcher = BinaryMatcher::patterns(["AA ?? CC"])
        .unwrap()
        .sub(move |addresses| {
            assert!(addresses.contains(&expected[0]));
            Ok(BinaryMatcher::address(expected[0]))
        })
        .offset(3)
        .first()
        .last();
    let image = scan_image();
    eval(&image, move |ctx| {
        Box::pin(async move {
            for _ in 0..2 {
                assert_eq!(matcher.resolve(ctx).await.unwrap(), [TEXT + 27]);
            }
        })
    });
}

#[test]
fn modify_with_chooses_a_different_offset_for_each_match_from_memory() {
    use crate::MemoryTrait;

    let matcher = patterns(&["AA ?? CC"])
        .with_context()
        .modify_with(|(ctx, address)| {
            let offset = if ctx.image().memory.u8(address + 1)? == 0xbb {
                1
            } else {
                2
            };
            address
                .checked_add(offset)
                .context("conditional offset overflow")
        });
    assert_eq!(
        run(&scan_image(), matcher).unwrap(),
        [TEXT + 9, TEXT + 26, TARGET + 1]
    );
}

#[test]
fn modify_with_preserves_callback_order_duplicates_and_absolute_addresses() {
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let matcher = patterns(&["AA DD CC", "AA BB CC", "AA DD CC"]).modify_with(move |address| {
        captured.lock().unwrap().push(address);
        // This is an absolute address, not an offset or a memory read.
        Ok(0)
    });
    assert_eq!(run(&scan_image(), matcher).unwrap(), [0, 0, 0, 0]);
    assert_eq!(
        *seen.lock().unwrap(),
        [TEXT + 24, TEXT + 8, TARGET, TEXT + 24]
    );
}

#[test]
fn modify_with_skips_callback_on_empty_input_or_inner_error() {
    let empty = patterns(&[]).modify_with(|_| panic!("empty input must skip callback"));
    assert!(run(&scan_image(), empty).unwrap().is_empty());
    let failed = BinaryMatcher::address(0)
        .offset(-1)
        .modify_with(|_| panic!("inner error must skip callback"));
    let error = run(&scan_image(), failed).unwrap_err();
    assert!(error.to_string().contains("address offset overflow"));
}

#[test]
fn modify_with_stops_at_first_callback_error() {
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let matcher = patterns(&["AA ?? CC"]).modify_with(move |address| {
        captured.lock().unwrap().push(address);
        if address == TEXT + 24 {
            bail_out!("unsupported instruction");
        }
        Ok(address)
    });
    assert_eq!(
        run(&scan_image(), matcher).unwrap_err().to_string(),
        "unsupported instruction"
    );
    assert_eq!(*seen.lock().unwrap(), [TEXT + 8, TEXT + 24]);
}

#[test]
fn modify_with_propagates_memory_and_checked_arithmetic_errors() {
    use crate::MemoryTrait;

    let unmapped = BinaryMatcher::address(0)
        .with_context()
        .modify_with(|(ctx, address)| Ok(u64::from(ctx.image().memory.u8(address)?)));
    let error = run(&scan_image(), unmapped).unwrap_err();
    assert_eq!(
        error,
        ResolveError::from(crate::MemoryAccessError::MemoryOutOfBoundsError)
    );
    let overflow = BinaryMatcher::address(u64::MAX).modify_with(|address| {
        address
            .checked_add(1)
            .context("conditional offset overflow")
    });
    assert_eq!(
        run(&scan_image(), overflow).unwrap_err().to_string(),
        "conditional offset overflow"
    );
}

#[test]
fn modify_with_errors_propagate_through_selectors_xrefs_and_sub() {
    let wrappers: [for<'a> fn(BinaryMatcher<'a>) -> BinaryMatcher<'a>; 3] = [
        |matcher| matcher.first(),
        |matcher| matcher.last(),
        |matcher| matcher.xrefs().addresses(),
    ];
    for wrap in wrappers {
        let matcher = wrap(
            BinaryMatcher::address(TEXT)
                .modify_with(|_| Err(ResolveError::new_msg("modify failed"))),
        );
        assert_eq!(
            run(&scan_image(), matcher).unwrap_err().to_string(),
            "modify failed"
        );
    }
    let matcher = BinaryMatcher::address(TEXT)
        .modify_with(|_| Err(ResolveError::new_msg("modify failed")))
        .sub(|_| panic!("sub must not run after modifier failure"));
    assert_eq!(
        run(&scan_image(), matcher).unwrap_err().to_string(),
        "modify failed"
    );
}

#[test]
fn modify_with_composes_with_nested_string_xrefs_and_sub_matchers() {
    use crate::MemoryTrait;

    let matcher = string_matcher(NARROW_TEXT, false)
        .xrefs()
        .xrefs()
        .addresses()
        .with_context()
        .modify_with(|(ctx, address)| {
            // SHARED_CALLER has a second call at +0x18; OTHER_CALLER has a NOP.
            let offset = if ctx.image().memory.u8(address + 0x18)? == 0xe8 {
                4
            } else {
                8
            };
            Ok(address + offset)
        })
        .sub(|addresses| {
            assert_eq!(addresses, [SHARED_CALLER + 4, OTHER_CALLER + 8]);
            Ok(BinaryMatcher::address(addresses[1]).modify_with(|address| Ok(address - 8)))
        })
        .xrefs()
        .first();
    assert_eq!(run(&string_graph(), matcher).unwrap(), [ENTRY]);
}

#[test]
fn filter_preserves_order_duplicates_and_passes_image_context() {
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let expected = vec![TEXT + 24, TEXT + 8, TARGET, TEXT + 24];
    let keep = vec![TEXT + 24, TARGET];
    let matcher = patterns(&["AA DD CC", "AA BB CC", "AA DD CC"])
        .with_context()
        .filter(move |(ctx, address)| {
            assert!(!ctx.image().memory.sections.is_empty());
            captured.lock().unwrap().push(address);
            keep.contains(&address)
        });
    assert_eq!(
        run(&scan_image(), matcher).unwrap(),
        [TEXT + 24, TARGET, TEXT + 24]
    );
    assert_eq!(*seen.lock().unwrap(), expected);
}

#[test]
fn filter_handles_all_none_empty_and_inner_errors() {
    for keep in [false, true] {
        let matcher = patterns(&["AA ?? CC"]).filter(move |_| keep);
        let expected = if keep {
            vec![TEXT + 8, TEXT + 24, TARGET]
        } else {
            vec![]
        };
        assert_eq!(run(&scan_image(), matcher).unwrap(), expected);
    }
    let empty = patterns(&[]).filter(|_| panic!("empty input must skip callback"));
    assert!(run(&scan_image(), empty).unwrap().is_empty());
    let failed = BinaryMatcher::address(0)
        .offset(-1)
        .filter(|_| panic!("inner error must skip callback"));
    assert!(
        run(&scan_image(), failed)
            .unwrap_err()
            .to_string()
            .contains("address offset overflow")
    );
}

#[test]
fn filter_modify_preserves_callback_order_duplicates_and_absolute_addresses() {
    use crate::MemoryTrait;
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let matcher = patterns(&["AA DD CC", "AA BB CC", "AA DD CC"])
        .with_context()
        .filter_modify(move |(ctx, address)| {
            captured.lock().unwrap().push(address);
            Ok((ctx.image().memory.u8(address + 1)? == 0xdd).then_some(0))
        });
    assert_eq!(run(&scan_image(), matcher).unwrap(), [0, 0]);
    assert_eq!(
        *seen.lock().unwrap(),
        [TEXT + 24, TEXT + 8, TARGET, TEXT + 24]
    );
}

#[test]
fn filter_modify_handles_all_none_empty_and_inner_errors() {
    let identity = patterns(&["AA ?? CC"]).filter_modify(|address| Ok(Some(address)));
    assert_eq!(
        run(&scan_image(), identity).unwrap(),
        [TEXT + 8, TEXT + 24, TARGET]
    );
    let none = patterns(&["AA ?? CC"]).filter_modify(|_| Ok(None::<u64>));
    assert!(run(&scan_image(), none).unwrap().is_empty());
    let empty = patterns(&[]).filter_modify::<u64, _>(|_| panic!("empty input must skip callback"));
    assert!(run(&scan_image(), empty).unwrap().is_empty());
    let failed = BinaryMatcher::address(0)
        .offset(-1)
        .filter_modify::<u64, _>(|_| panic!("inner error must skip callback"));
    assert!(
        run(&scan_image(), failed)
            .unwrap_err()
            .to_string()
            .contains("address offset overflow")
    );
}

#[test]
fn filter_modify_stops_at_first_error_after_dropped_results() {
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let matcher = patterns(&["AA ?? CC"]).filter_modify(move |address| {
        captured.lock().unwrap().push(address);
        if address == TEXT + 24 {
            bail_out!("filter modification failed");
        }
        Ok(None::<u64>)
    });
    assert_eq!(
        run(&scan_image(), matcher).unwrap_err().to_string(),
        "filter modification failed"
    );
    assert_eq!(*seen.lock().unwrap(), [TEXT + 8, TEXT + 24]);
}

#[test]
fn filter_modify_propagates_memory_errors_through_filter_and_sub() {
    use crate::MemoryTrait;

    let matcher = BinaryMatcher::address(0)
        .with_context()
        .filter_modify(|(ctx, address)| Ok(Some(u64::from(ctx.image().memory.u8(address)?))))
        .filter(|_| panic!("filter must not run after error"))
        .sub(|_| panic!("sub must not run after error"));
    assert_eq!(
        run(&scan_image(), matcher).unwrap_err(),
        ResolveError::from(crate::MemoryAccessError::MemoryOutOfBoundsError)
    );
}

#[test]
fn filter_and_filter_modify_compose_with_modifiers_xrefs_and_selectors() {
    let matcher = string_matcher(NARROW_TEXT, false)
        .xrefs()
        .xrefs()
        .filter(|function| function.start == SHARED_CALLER)
        .addresses()
        .modify_with(|address| Ok(address + 4))
        .filter_modify(|address| Ok(Some(address - 4)))
        .xrefs()
        .first()
        .last();
    assert_eq!(run(&string_graph(), matcher).unwrap(), [ENTRY]);
}

#[test]
fn address_preserves_full_width_and_does_not_require_mapped_memory() {
    let image = image(vec![]);
    for address in [0, TEXT, u64::MAX] {
        assert_eq!(
            run(&image, BinaryMatcher::address(address)).unwrap(),
            [address]
        );
    }
}

#[test]
fn patterns_scan_all_sections_with_wildcards() {
    assert_eq!(
        run(&scan_image(), patterns(&["AA ?? CC"])).unwrap(),
        [TEXT + 8, TEXT + 24, TARGET]
    );
}

#[test]
fn patterns_preserve_pattern_order_and_duplicates() {
    assert_eq!(
        run(
            &scan_image(),
            patterns(&["AA DD CC", "AA BB CC", "AA DD CC"])
        )
        .unwrap(),
        [TEXT + 24, TEXT + 8, TARGET, TEXT + 24]
    );
}

#[test]
fn empty_and_unmatched_patterns_return_empty() {
    for matcher in [patterns(&[]), patterns(&["DE AD BE EF"])] {
        assert!(run(&scan_image(), matcher).unwrap().is_empty());
    }
}

#[test]
fn first_and_last_follow_result_order_not_address_order() {
    let first = patterns(&["AA DD CC", "AA BB CC"]).first();
    let last = patterns(&["AA BB CC", "AA DD CC"]).last();
    assert_eq!(run(&scan_image(), first).unwrap(), [TEXT + 24]);
    assert_eq!(run(&scan_image(), last).unwrap(), [TEXT + 24]);
}

#[test]
fn selectors_accept_empty_and_singleton_inputs() {
    for wrap in [BinaryMatcher::first, BinaryMatcher::last] {
        assert!(run(&scan_image(), wrap(patterns(&[]))).unwrap().is_empty());
        assert_eq!(
            run(&scan_image(), wrap(BinaryMatcher::address(TEXT))).unwrap(),
            [TEXT]
        );
    }
}

#[test]
fn offsets_apply_to_every_match_and_compose() {
    let matcher = patterns(&["AA ?? CC"]).offset(7).offset(-3);
    assert_eq!(
        run(&scan_image(), matcher).unwrap(),
        [TEXT + 12, TEXT + 28, TARGET + 4]
    );
    assert_eq!(
        run(&scan_image(), BinaryMatcher::address(0).offset(0)).unwrap(),
        [0]
    );
    assert!(
        run(&scan_image(), patterns(&[]).offset(-1))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn offsets_check_unsigned_bounds_including_minimum_signed_offset() {
    let image = image(vec![]);
    for (address, displacement, expected) in [
        (u64::MAX - 1, 1, u64::MAX),
        (1, -1, 0),
        (1u64 << 63, i64::MIN, 0),
    ] {
        assert_eq!(
            run(&image, BinaryMatcher::address(address).offset(displacement)).unwrap(),
            [expected]
        );
    }
    for (address, displacement) in [(u64::MAX, 1), (0, -1), (0, i64::MIN)] {
        let error = run(&image, BinaryMatcher::address(address).offset(displacement)).unwrap_err();
        assert!(error.to_string().contains("address offset overflow"));
    }
}

#[test]
fn sub_receives_results_and_context_then_evaluates_returned_matcher() {
    let matcher = patterns(&["AA DD CC"])
        .with_context()
        .sub(|(ctx, results)| {
            assert_eq!(results, [TEXT + 24]);
            assert_eq!(ctx.image().base_address, BASE);
            Ok(patterns(&["AA BB CC"]).first())
        });
    assert_eq!(run(&scan_image(), matcher).unwrap(), [TEXT + 8]);
}

#[test]
fn sub_runs_on_empty_input_and_can_return_another_sub() {
    let matcher = patterns(&[]).sub(|results| {
        assert!(results.is_empty());
        Ok(BinaryMatcher::address(42)
            .sub(|results| Ok(BinaryMatcher::address(results[0]).offset(-2))))
    });
    assert_eq!(run(&scan_image(), matcher).unwrap(), [40]);
}

#[test]
fn sub_propagates_callback_and_returned_matcher_errors() {
    let callback_error =
        BinaryMatcher::address(1).sub(|_| Err(ResolveError::new_msg("callback failed")));
    assert_eq!(
        run(&scan_image(), callback_error).unwrap_err().to_string(),
        "callback failed"
    );
    let returned_error = patterns(&[]).sub(|_| Ok(BinaryMatcher::address(0).offset(-1)));
    assert!(run(&scan_image(), returned_error).is_err());
}

#[test]
fn all_wrappers_propagate_inner_errors() {
    let wrappers: [for<'a> fn(BinaryMatcher<'a>) -> BinaryMatcher<'a>; 3] = [
        |matcher| matcher.first(),
        |matcher| matcher.last(),
        |matcher| matcher.xrefs().addresses(),
    ];
    for wrap in wrappers {
        assert!(run(&scan_image(), wrap(BinaryMatcher::address(0).offset(-1))).is_err());
    }
    assert!(
        run(
            &scan_image(),
            BinaryMatcher::address(0).offset(-1).offset(1)
        )
        .is_err()
    );
    let matcher = BinaryMatcher::address(0)
        .offset(-1)
        .sub(|_| panic!("callback must not run after an inner error"));
    assert!(run(&scan_image(), matcher).is_err());
}

fn relative(text: &mut [u8], at: usize, opcode: &[u8], target: u64) {
    text[at..at + opcode.len()].copy_from_slice(opcode);
    let next = TEXT + (at + opcode.len() + 4) as u64;
    let displacement = i32::try_from(i128::from(target) - i128::from(next)).unwrap();
    text[at + opcode.len()..at + opcode.len() + 4].copy_from_slice(&displacement.to_le_bytes());
}

fn xrefs<'scope>(target: u64) -> BinaryMatcher<'scope> {
    BinaryMatcher::address(target).xrefs().addresses()
}

#[test]
fn references_and_callers_return_sites_without_mapping_to_functions() {
    let mut text = vec![0x90; 256];
    relative(&mut text, 8, &[0x48, 0x8d, 0x0d], TARGET);
    relative(&mut text, 24, &[0x4c, 0x8d, 0x05], TARGET);
    relative(&mut text, 72, &[0xe8], TARGET);
    relative(&mut text, 88, &[0xe9], TARGET);
    relative(&mut text, 144, &[0xe8], TARGET);
    let image = image(text);
    assert_eq!(
        run(&image, BinaryMatcher::address(TARGET).references()).unwrap(),
        [TEXT + 8, TEXT + 24]
    );
    assert_eq!(
        run(&image, BinaryMatcher::address(TARGET).callers()).unwrap(),
        [TEXT + 72, TEXT + 144, TEXT + 88]
    );
    assert_eq!(
        run(
            &image,
            BinaryMatcher::address(TARGET).references().root_functions()
        )
        .unwrap(),
        [TEXT, TEXT]
    );
    assert_eq!(
        run(
            &image,
            BinaryMatcher::address(TARGET).callers().root_functions()
        )
        .unwrap(),
        [TEXT + 64, TEXT + 64]
    );
    assert_eq!(
        run(&image, xrefs(TARGET)).unwrap(),
        [TEXT, TEXT, TEXT + 64, TEXT + 64]
    );
}

#[test]
fn references_and_callers_retain_indirect_pointer_scan_behavior() {
    let pointer = BASE + 0x5000;
    let mut text = vec![0x90; 256];
    relative(&mut text, 8, &[0x48, 0x8d, 0x0d], pointer);
    relative(&mut text, 72, &[0xe8], pointer);
    let mut image = image(text);
    image.memory.sections.push(section(
        ".data",
        pointer,
        SectionKind::Data,
        TARGET.to_le_bytes().to_vec(),
    ));
    assert_eq!(
        run(&image, BinaryMatcher::address(TARGET).references()).unwrap(),
        [TEXT + 8]
    );
    assert_eq!(
        run(&image, BinaryMatcher::address(TARGET).callers()).unwrap(),
        [TEXT + 72]
    );
}

#[test]
fn function_entries_retain_bounds_size_and_backend_metadata() {
    let image = scan_image();
    let matcher = Matcher::from_sync(|()| Ok(vec![TEXT + 24, TEXT + 64, TEXT, 0]))
        .then(stages::root_functions());
    let functions = run_stage(&image, matcher, ()).unwrap();
    assert_eq!(functions.len(), 3);
    for (function, start) in functions.iter().zip([TEXT, TEXT + 64, TEXT]) {
        assert_eq!(function.start, start);
        assert_eq!(function.end, start + 0x40);
        assert_eq!(function.size(), 0x40);
        assert_eq!(function.address(), start);
        let metadata = image.get_root_function(start).unwrap().unwrap();
        assert_eq!(
            function.unwind,
            (metadata.unwind != 0).then_some(metadata.unwind)
        );
    }
    assert_eq!(functions[0], functions[2]);

    let matcher = BinaryMatcher::address(TEXT + 24)
        .then(stages::root_functions())
        .filter(|function| function.size() == 0x40)
        .map_each(|function| function.start);
    assert_eq!(run(&image, matcher).unwrap(), [TEXT]);
}

#[test]
fn fluent_traversal_returns_typed_nodes_and_preserves_metadata() {
    let image = string_graph();
    let references: Matcher<'_, (), Vec<InstructionSite>> =
        string_matcher(NARROW_TEXT, false).references();
    let roots: Matcher<'_, (), Vec<FunctionEntry>> = references.root_functions();
    let functions = run_stage(&image, roots.clone(), ()).unwrap();
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].start, READER_A);
    assert_eq!(functions[0].end, READER_A + 0x40);
    assert_eq!(functions[0].size(), 0x40);

    let callers: Matcher<'_, (), Vec<InstructionSite>> = roots.callers();
    let callers = callers
        .root_functions()
        .filter(|function| function.start == SHARED_CALLER);
    let callees: Matcher<'_, (), Vec<Address>> = callers.clone().callees();
    assert_eq!(
        run_stage(&image, callees, ()).unwrap(),
        [Address(READER_A), Address(READER_B)]
    );
    let sites: Matcher<'_, (), Vec<InstructionSite>> = callers.call_sites();
    assert_eq!(
        run_stage(&image, sites, ()).unwrap(),
        [
            InstructionSite(SHARED_CALLER + 8),
            InstructionSite(SHARED_CALLER + 0x18)
        ]
    );

    let xrefs: Matcher<'_, (), Vec<FunctionEntry>> = string_matcher(NARROW_TEXT, false).xrefs();
    assert_eq!(run_stage(&image, xrefs, ()).unwrap(), functions);
}

#[test]
fn typed_nodes_convert_to_addresses_without_reordering_or_deduplicating() {
    let matcher = Matcher::from_sync(|()| {
        Ok(vec![
            FunctionEntry {
                start: TEXT + 64,
                end: TEXT + 128,
                unwind: None,
            },
            FunctionEntry {
                start: TEXT,
                end: TEXT + 64,
                unwind: None,
            },
            FunctionEntry {
                start: TEXT + 64,
                end: TEXT + 128,
                unwind: None,
            },
        ])
    })
    .addresses();
    assert_eq!(
        run_stage(&scan_image(), matcher, ()).unwrap(),
        [TEXT + 64, TEXT, TEXT + 64]
    );
    assert_eq!(std::mem::size_of::<Address>(), std::mem::size_of::<u64>());
    assert_eq!(
        std::mem::size_of::<InstructionSite>(),
        std::mem::size_of::<u64>()
    );
}

#[test]
fn root_functions_maps_entries_and_interiors_preserving_order_and_duplicates() {
    let matcher = patterns(&["AA DD CC", "AA BB CC", "AA DD CC"]).root_functions();
    assert_eq!(run(&scan_image(), matcher).unwrap(), [TEXT, TEXT, TEXT]);
    for address in [TEXT, TEXT + 8, TEXT + 63, TEXT + 64] {
        assert_eq!(
            run(
                &scan_image(),
                BinaryMatcher::address(address).root_functions()
            )
            .unwrap(),
            [if address < TEXT + 64 { TEXT } else { TEXT + 64 }]
        );
    }
}

#[test]
fn callees_and_call_sites_use_the_containing_function_entry() {
    let image = string_graph();
    for address in [SHARED_CALLER, SHARED_CALLER + 8, SHARED_CALLER + 0x30] {
        assert_eq!(
            run(&image, BinaryMatcher::address(address).callees()).unwrap(),
            [READER_A, READER_B]
        );
        assert_eq!(
            run(&image, BinaryMatcher::address(address).call_sites()).unwrap(),
            [SHARED_CALLER + 8, SHARED_CALLER + 0x18]
        );
    }
    // ENTRY also tail-jumps to OTHER_CALLER; it is not a direct call.
    assert_eq!(
        run(&image, BinaryMatcher::address(ENTRY).callees()).unwrap(),
        [SHARED_CALLER]
    );
}

#[test]
fn callees_preserve_input_order_duplicate_targets_and_recursive_calls() {
    let mut image = string_graph();
    let text = image.memory.sections[0].data.to_mut();
    relative(text, 0x98, &[0xe8], READER_A);
    relative(text, 0xa8, &[0xe8], SHARED_CALLER);
    let matcher = patterns(&["E8 ?? ?? ?? ??"])
        .filter(|address| [SHARED_CALLER + 8, SHARED_CALLER + 0x18].contains(&address))
        .callees();
    assert_eq!(
        run(&image, matcher).unwrap(),
        [
            READER_A,
            READER_A,
            SHARED_CALLER,
            READER_A,
            READER_A,
            SHARED_CALLER
        ]
    );
}

#[test]
fn outgoing_calls_follow_local_branches_but_not_external_branches_or_dead_code() {
    let mut text = vec![0x90; 256];
    // Visit the taken block before the fallthrough block, but return sites sorted by IP.
    relative(&mut text, 0, &[0x0f, 0x84], TEXT + 0x20);
    relative(&mut text, 8, &[0xe8], TARGET);
    text[13] = 0xc3;
    relative(&mut text, 16, &[0xe8], TARGET + 8); // Unreachable after ret.
    relative(&mut text, 0x20, &[0xe8], TARGET + 16);
    relative(&mut text, 0x28, &[0xe9], TEXT); // Loop, visited only once.
    let image = image(text);
    assert_eq!(
        run(&image, BinaryMatcher::address(TEXT).call_sites()).unwrap(),
        [TEXT + 8, TEXT + 0x20]
    );
    assert_eq!(
        run(&image, BinaryMatcher::address(TEXT).callees()).unwrap(),
        [TARGET, TARGET + 16]
    );
}

#[test]
fn outgoing_calls_exclude_indirect_calls_and_do_not_scan_other_functions() {
    let mut text = vec![0x90; 256];
    text[..2].copy_from_slice(&[0xff, 0xd0]); // call rax: no statically known target.
    relative(&mut text, 8, &[0xe8], TARGET);
    // A conditional branch to an unmapped address must not be followed.
    relative(&mut text, 16, &[0x0f, 0x84], TARGET);
    relative(&mut text, 24, &[0xe9], TEXT + 64);
    relative(&mut text, 72, &[0xe8], TARGET + 8);
    let image = image(text);
    assert_eq!(
        run(&image, BinaryMatcher::address(TEXT).callees()).unwrap(),
        [TARGET]
    );
    assert_eq!(
        run(&image, BinaryMatcher::address(TEXT).call_sites()).unwrap(),
        [TEXT + 8]
    );
}

#[test]
fn outgoing_calls_stop_at_function_boundary_and_indirect_jumps() {
    let mut text = vec![0x90; 256];
    relative(&mut text, 8, &[0xe8], TARGET);
    relative(&mut text, 72, &[0xe8], TARGET + 8);
    assert_eq!(
        run(&image(text.clone()), BinaryMatcher::address(TEXT).callees()).unwrap(),
        [TARGET]
    );
    text[0..2].copy_from_slice(&[0xff, 0xe0]); // jmp rax.
    assert!(
        run(&image(text), BinaryMatcher::address(TEXT).callees())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn traversal_handles_empty_unknown_and_unreferenced_inputs() {
    let wrappers: [for<'a> fn(BinaryMatcher<'a>) -> BinaryMatcher<'a>; 5] = [
        |matcher: BinaryMatcher| matcher.references().addresses(),
        |matcher: BinaryMatcher| matcher.callers().addresses(),
        |matcher: BinaryMatcher| matcher.root_functions().addresses(),
        |matcher: BinaryMatcher| matcher.callees().addresses(),
        |matcher: BinaryMatcher| matcher.call_sites().addresses(),
    ];
    for wrap in wrappers {
        assert!(run(&scan_image(), wrap(patterns(&[]))).unwrap().is_empty());
        assert!(
            run(&scan_image(), wrap(BinaryMatcher::address(0)))
                .unwrap()
                .is_empty()
        );
        let error = run(&scan_image(), wrap(BinaryMatcher::address(0).offset(-1))).unwrap_err();
        assert!(error.to_string().contains("address offset overflow"));
    }
}

#[test]
fn function_traversal_accepts_an_empty_function_table() {
    let image = construct_image(vec![0x90; 64], 0);
    let wrappers: [for<'a> fn(BinaryMatcher<'a>) -> BinaryMatcher<'a>; 3] = [
        |matcher: BinaryMatcher| matcher.root_functions().addresses(),
        |matcher: BinaryMatcher| matcher.callees().addresses(),
        |matcher: BinaryMatcher| matcher.call_sites().addresses(),
    ];
    for wrap in wrappers {
        assert!(
            run(&image, wrap(BinaryMatcher::address(TEXT)))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn outgoing_calls_propagate_memory_and_invalid_instruction_errors() {
    let wrappers: [for<'a> fn(BinaryMatcher<'a>) -> BinaryMatcher<'a>; 2] = [
        |matcher: BinaryMatcher| matcher.callees().addresses(),
        |matcher: BinaryMatcher| matcher.call_sites().addresses(),
    ];
    for wrap in wrappers {
        let mut missing = string_graph();
        missing.memory.sections.retain(|s| s.name() != ".text");
        let error = run(&missing, wrap(BinaryMatcher::address(TEXT))).unwrap_err();
        assert_eq!(
            error,
            ResolveError::from(crate::MemoryAccessError::MemoryOutOfBoundsError)
        );
        let error = run(&image(vec![0x0f]), wrap(BinaryMatcher::address(TEXT))).unwrap_err();
        assert!(error.to_string().contains("invalid instruction"));
    }
}

#[test]
fn traversal_composes_from_strings_through_sites_roots_and_targets() {
    let matcher = string_matcher(NARROW_TEXT, false)
        .references()
        .root_functions()
        .callers()
        .root_functions()
        .filter(|function| function.start == SHARED_CALLER)
        .callees();
    assert_eq!(run(&string_graph(), matcher).unwrap(), [READER_A, READER_B]);
}

#[test]
fn xrefs_resolve_lea_calls_and_jumps_to_containing_roots() {
    let mut text = vec![0x90; 256];
    relative(&mut text, 8, &[0x48, 0x8d, 0x0d], TARGET);
    relative(&mut text, 24, &[0x4c, 0x8d, 0x05], TARGET);
    relative(&mut text, 72, &[0xe8], TARGET);
    relative(&mut text, 88, &[0xe9], TARGET);
    assert_eq!(
        run(&image(text), xrefs(TARGET)).unwrap(),
        [TEXT, TEXT, TEXT + 64, TEXT + 64]
    );
}

#[test]
fn xrefs_find_indirect_pointer_references() {
    let pointer = BASE + 0x5000;
    let mut text = vec![0x90; 256];
    relative(&mut text, 8, &[0x48, 0x8d, 0x0d], pointer);
    relative(&mut text, 72, &[0xe8], pointer);
    let mut image = image(text);
    image.memory.sections.push(section(
        ".data",
        pointer,
        SectionKind::Data,
        TARGET.to_le_bytes().to_vec(),
    ));
    assert_eq!(run(&image, xrefs(TARGET)).unwrap(), [TEXT, TEXT + 64]);
}

#[test]
fn xrefs_find_immediate_32_bit_addresses() {
    let mut text = vec![0x90; 256];
    text[8] = 0xb9;
    text[9..13].copy_from_slice(&0x12345678u32.to_le_bytes());
    assert_eq!(run(&image(text), xrefs(0x12345678)).unwrap(), [TEXT]);
}

#[test]
fn xrefs_ignore_references_outside_known_functions() {
    let mut text = vec![0x90; 256];
    relative(&mut text, 144, &[0xe8], TARGET);
    assert!(run(&image(text), xrefs(TARGET)).unwrap().is_empty());
}

#[test]
fn xrefs_accept_empty_and_unreferenced_targets() {
    let image = image(vec![0x90; 256]);
    assert!(run(&image, xrefs(TARGET)).unwrap().is_empty());
    assert!(run(&image, patterns(&[]).xrefs()).unwrap().is_empty());
}

#[test]
fn nested_pattern_xref_selector_and_offset_pipeline() {
    let mut text = vec![0x90; 256];
    relative(&mut text, 72, &[0xe8], TARGET);
    let mut image = image(text);
    image.memory.sections.push(section(
        ".data",
        TARGET,
        SectionKind::Data,
        vec![0xde, 0xad, 0xbe, 0xef],
    ));
    let matcher = patterns(&["DE AD BE EF"])
        .xrefs()
        .first()
        .addresses()
        .offset(4);
    assert_eq!(run(&image, matcher).unwrap(), [TEXT + 68]);
}

const WIDE_STRING: u64 = TARGET + 0x80;
const READER_A: u64 = TEXT;
const READER_B: u64 = TEXT + 0x40;
const SHARED_CALLER: u64 = TEXT + 0x80;
const OTHER_CALLER: u64 = TEXT + 0xc0;
const ENTRY: u64 = TEXT + 0x100;
const NARROW_TEXT: &str = "Load asset: café 🦀";
const WIDE_TEXT: &str = "解析対象 🦀";

fn string_matcher<'scope>(value: &str, wide: bool) -> BinaryMatcher<'scope> {
    let terminated = format!("{value}\0");
    BinaryMatcher::pattern(if wide {
        util::utf16_pattern(&terminated)
    } else {
        util::utf8_pattern(&terminated)
    })
}

// Graph (arrows mean "references"):
// ENTRY -> SHARED_CALLER -> READER_A -> UTF-8 string
//       -> OTHER_CALLER  -> READER_A
//          SHARED_CALLER -> READER_B -> UTF-16 string
// A sixth function references a similar, nonmatching string.
fn string_graph() -> Image<'static> {
    let mut text = vec![0x90; 0x180];
    for end in (0x3f..0x180).step_by(0x40) {
        text[end] = 0xc3;
    }
    relative(&mut text, 8, &[0x48, 0x8d, 0x0d], TARGET);
    relative(&mut text, 0x48, &[0x4c, 0x8d, 0x05], WIDE_STRING);
    relative(&mut text, 0x88, &[0xe8], READER_A);
    relative(&mut text, 0x98, &[0xe8], READER_B);
    relative(&mut text, 0xc8, &[0xe8], READER_A);
    relative(&mut text, 0x108, &[0xe8], SHARED_CALLER);
    relative(&mut text, 0x118, &[0xe9], OTHER_CALLER);
    relative(&mut text, 0x148, &[0x48, 0x8d, 0x0d], TARGET + 0x100);

    let mut image = construct_image(text, 6);

    let mut strings = vec![0; 0x180];
    let narrow = format!("{NARROW_TEXT}\0").into_bytes();
    strings[..narrow.len()].copy_from_slice(&narrow);
    let wide = util::utf16(&format!("{WIDE_TEXT}\0"));
    strings[0x80..0x80 + wide.len()].copy_from_slice(&wide);
    // Prefix matches must not accept this longer string or its referencing function.
    let decoy = format!("{NARROW_TEXT} (decoy)\0").into_bytes();
    strings[0x100..0x100 + decoy.len()].copy_from_slice(&decoy);
    image.memory.sections.push(section(
        ".rdata",
        TARGET,
        SectionKind::ReadOnlyData,
        strings,
    ));
    image
}

#[test]
fn nested_xrefs_walk_unicode_string_readers_callers_and_entry() {
    let image = string_graph();
    for (value, wide, address, reader, callers) in [
        (
            NARROW_TEXT,
            false,
            TARGET,
            READER_A,
            vec![SHARED_CALLER, OTHER_CALLER],
        ),
        (WIDE_TEXT, true, WIDE_STRING, READER_B, vec![SHARED_CALLER]),
    ] {
        assert_eq!(run(&image, string_matcher(value, wide)).unwrap(), [address]);
        assert_eq!(
            run(&image, string_matcher(value, wide).xrefs()).unwrap(),
            [reader]
        );
        assert_eq!(
            run(&image, string_matcher(value, wide).xrefs().xrefs()).unwrap(),
            callers
        );
        // Duplicate roots are retained when ENTRY references both narrow-string callers.
        assert_eq!(
            run(&image, string_matcher(value, wide).xrefs().xrefs().xrefs()).unwrap(),
            vec![ENTRY; callers.len()]
        );
    }
}

#[test]
fn sub_reads_string_then_builds_a_different_string_xref_pipeline() {
    use crate::MemoryTrait;

    let matcher = string_matcher(NARROW_TEXT, false)
        .with_context()
        .sub(|(ctx, addresses)| {
            if addresses.len() != 1 {
                bail_out!("expected one selector string");
            }
            let value = ctx.image().memory.read_string(addresses[0])?;
            if value != NARROW_TEXT {
                bail_out!("wrong selector string");
            }
            Ok(string_matcher(WIDE_TEXT, true)
                .xrefs()
                .addresses()
                .sub(|readers| {
                    assert_eq!(readers, [READER_B]);
                    Ok(BinaryMatcher::address(readers[0]).xrefs().addresses())
                }))
        });
    assert_eq!(run(&string_graph(), matcher).unwrap(), [SHARED_CALLER]);
}

fn common_string_caller<'scope>(narrow: &str, wide: &str) -> BinaryMatcher<'scope> {
    // The callback is Fn, so reconstruct the returned matcher from captured text.
    let wide = wide.to_owned();
    string_matcher(narrow, false)
        .xrefs()
        .xrefs()
        .addresses()
        .sub(move |narrow_callers| {
            Ok(string_matcher(&wide, true)
                .xrefs()
                .xrefs()
                .addresses()
                .sub(move |wide_callers| {
                    let mut common: Vec<_> = narrow_callers
                        .iter()
                        .copied()
                        .filter(|address| wide_callers.contains(address))
                        .collect();
                    common.sort_unstable();
                    common.dedup();
                    if common.len() != 1 {
                        bail_out!(format!(
                            "expected one common caller, found {}",
                            common.len()
                        ));
                    }
                    Ok(BinaryMatcher::address(common[0]))
                }))
        })
}

#[test]
fn nested_sub_matchers_intersect_string_callers_then_find_their_entry() {
    let matcher = common_string_caller(NARROW_TEXT, WIDE_TEXT)
        .xrefs()
        .first()
        .addresses()
        .offset(4);
    assert_eq!(run(&string_graph(), matcher).unwrap(), [ENTRY + 4]);
}

#[test]
fn nested_sub_matchers_reject_missing_strings_in_either_branch() {
    for (narrow, wide) in [("missing", WIDE_TEXT), (NARROW_TEXT, "missing")] {
        let error = run(&string_graph(), common_string_caller(narrow, wide).xrefs()).unwrap_err();
        assert_eq!(error.to_string(), "expected one common caller, found 0");
    }
}

#[test]
fn nested_sub_matchers_reject_ambiguous_shared_string_callers() {
    let mut image = string_graph();
    let text = image.memory.sections[0].data.to_mut();
    relative(text, 0xd8, &[0xe8], READER_B);
    let error = run(&image, common_string_caller(NARROW_TEXT, WIDE_TEXT).xrefs()).unwrap_err();
    assert_eq!(error.to_string(), "expected one common caller, found 2");
}

#[test]
fn two_strings_example_finds_all_common_readers_and_removes_duplicates() {
    let mut image = string_graph();
    let text = image.memory.sections[0].data.to_mut();
    relative(text, 0x18, &[0x48, 0x8d, 0x0d], TARGET);
    relative(text, 0x28, &[0x48, 0x8d, 0x0d], TARGET + 0x100);
    relative(text, 0x58, &[0x48, 0x8d, 0x0d], TARGET);
    relative(text, 0x68, &[0x48, 0x8d, 0x0d], TARGET + 0x100);
    let decoy = format!("{NARROW_TEXT} (decoy)");
    assert_eq!(
        two_strings_example::methods_referencing_both(&image, NARROW_TEXT, &decoy).unwrap(),
        [READER_A, READER_B]
    );
}

#[test]
fn two_strings_example_does_not_confuse_callers_with_shared_readers() {
    let mut image = string_graph();
    // SHARED_CALLER calls one reader for each literal, but references neither
    // literal itself. It must not appear in the result.
    relative(
        image.memory.sections[0].data.to_mut(),
        0xa8,
        &[0xe8],
        TEXT + 0x140,
    );
    let decoy = format!("{NARROW_TEXT} (decoy)");
    assert!(
        two_strings_example::methods_referencing_both(&image, NARROW_TEXT, &decoy)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn two_strings_example_handles_missing_and_identical_literals() {
    let image = string_graph();
    for (first, second) in [("missing", NARROW_TEXT), (NARROW_TEXT, "missing")] {
        assert!(
            two_strings_example::methods_referencing_both(&image, first, second)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        two_strings_example::methods_referencing_both(&image, NARROW_TEXT, NARROW_TEXT).unwrap(),
        [READER_A]
    );
}
