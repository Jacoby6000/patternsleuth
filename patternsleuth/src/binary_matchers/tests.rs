use super::*;

const BASE: u64 = 0x1_4000_0000;
const TEXT: u64 = BASE + 0x1000;
const TARGET: u64 = BASE + 0x4000;

mod fixtures;

#[cfg(feature = "image-pe")]
mod pe {
    use super::fixtures::pe as construct_image;
    include!("tests/shared.rs");

    #[test]
    fn xrefs_propagate_unwind_memory_errors() {
        let mut text = vec![0x90; 256];
        relative(&mut text, 8, &[0xe8], TARGET);
        let mut image = image(text);
        image.memory.sections.retain(|s| s.name() != ".xdata");
        assert!(run(&image, xrefs(TARGET)).is_err());
    }

    #[test]
    fn function_traversal_propagates_unwind_memory_errors() {
        let mut image = string_graph();
        image.memory.sections.retain(|s| s.name() != ".xdata");
        let wrappers: [for<'a> fn(BinaryMatcher<'a>) -> BinaryMatcher<'a>; 3] = [
            |matcher: BinaryMatcher| matcher.root_functions().addresses(),
            |matcher: BinaryMatcher| matcher.callees().addresses(),
            |matcher: BinaryMatcher| matcher.call_sites().addresses(),
        ];
        for wrap in wrappers {
            let error = run(&image, wrap(BinaryMatcher::address(SHARED_CALLER))).unwrap_err();
            assert_eq!(
                error.to_string(),
                crate::MemoryAccessError::MemoryOutOfBoundsError.to_string()
            );
        }
        // Raw reference scans do not require unwind metadata.
        assert_eq!(
            run(&image, BinaryMatcher::address(TARGET).references()).unwrap(),
            [TEXT + 8]
        );
        assert_eq!(
            run(&image, BinaryMatcher::address(SHARED_CALLER).callers()).unwrap(),
            [ENTRY + 8]
        );
    }

    #[test]
    fn nested_string_pipeline_propagates_xref_errors_before_sub_callback() {
        let mut image = string_graph();
        image.memory.sections.retain(|s| s.name() != ".xdata");
        let matcher = string_matcher(NARROW_TEXT, false)
            .xrefs()
            .xrefs()
            .addresses()
            .sub(|_| panic!("callback must not run after a nested xref error"));
        let error = run(&image, matcher).unwrap_err();
        assert_eq!(
            error,
            ResolveError::from(crate::MemoryAccessError::MemoryOutOfBoundsError)
        );
    }
}

#[cfg(feature = "image-elf")]
mod elf {
    use super::fixtures::elf as construct_image;
    use crate::image::ImageType;
    include!("tests/shared.rs");
    include!("tests/elf_parsed.rs");
}
