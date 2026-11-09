// Minimal linked x86-64 ELF built in memory: no compiler, external binaries, or
// fixture-generation step is required. File offsets equal addresses minus BASE,
// so the same bytes also model a mapped PT_LOAD segment.
fn elf_file() -> Vec<u8> {
    fn put(data: &mut [u8], at: usize, bytes: &[u8]) {
        data[at..at + bytes.len()].copy_from_slice(bytes);
    }

    let mut data = vec![0; 0x5180];
    data[..16].copy_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    for (at, value) in [
        (16, 2u16),
        (18, 62),
        (52, 64),
        (54, 56),
        (56, 2),
        (58, 64),
        (60, 6),
        (62, 5),
    ] {
        put(&mut data, at, &value.to_le_bytes());
    }
    put(&mut data, 20, &1u32.to_le_bytes());
    for (at, value) in [(24, ENTRY), (32, 64), (40, 0x5000)] {
        put(&mut data, at, &value.to_le_bytes());
    }

    // PT_LOAD plus PT_GNU_EH_FRAME for the mapped-image loader.
    for (at, kind, flags, offset, size) in [
        (64, object::elf::PT_LOAD, 5u32, 0u64, 0x5000u64),
        (120, object::elf::PT_GNU_EH_FRAME, 4, 0x3000, 16),
    ] {
        put(&mut data, at, &kind.to_le_bytes());
        put(&mut data, at + 4, &flags.to_le_bytes());
        for (field, value) in [
            (8, offset),
            (16, BASE + offset),
            (24, BASE + offset),
            (32, size),
            (40, size),
            (48, 0x1000),
        ] {
            put(&mut data, at + field, &value.to_le_bytes());
        }
    }

    let graph = string_graph();
    put(&mut data, 0x1000, graph.memory.sections[0].data());
    put(
        &mut data,
        0x4000,
        graph.memory.sections.last().unwrap().data(),
    );

    // .eh_frame: version-1 CIE, absolute 64-bit FDE locations, six functions.
    // CIE: id=0, empty augmentation, code alignment=1, data alignment=-8,
    // return register=16 (RIP), initial CFA=RSP+8.
    let mut frames = vec![12, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0x78, 16, 0x0c, 7, 8];
    for i in 0..6u64 {
        let cie_pointer = frames.len() as u32 + 4;
        frames.extend(20u32.to_le_bytes());
        frames.extend(cie_pointer.to_le_bytes());
        frames.extend((TEXT + i * 0x40).to_le_bytes());
        frames.extend(0x40u64.to_le_bytes());
    }
    frames.extend(0u32.to_le_bytes());
    put(&mut data, 0x2000, &frames);

    // .eh_frame_hdr: version 1, absolute pointer, udata4 count, omitted table.
    put(&mut data, 0x3000, &[1, 0, 3, 0xff]);
    put(&mut data, 0x3004, &(BASE + 0x2000).to_le_bytes());
    put(&mut data, 0x300c, &6u32.to_le_bytes());

    let names = b"\0.text\0.rodata\0.eh_frame\0.eh_frame_hdr\0.shstrtab\0";
    put(&mut data, 0x3200, names);
    // Section zero is null; each tuple describes one remaining ELF64 shdr.
    for (i, (name, kind, flags, offset, size)) in [
        (1u32, object::elf::SHT_PROGBITS, 6u64, 0x1000u64, 0x180u64),
        (7, object::elf::SHT_PROGBITS, 2, 0x4000, 0x180),
        (
            15,
            object::elf::SHT_PROGBITS,
            2,
            0x2000,
            frames.len() as u64,
        ),
        (25, object::elf::SHT_PROGBITS, 2, 0x3000, 16),
        (39, object::elf::SHT_STRTAB, 0, 0x3200, names.len() as u64),
    ]
    .into_iter()
    .enumerate()
    {
        let at = 0x5000 + (i + 1) * 64;
        put(&mut data, at, &name.to_le_bytes());
        put(&mut data, at + 4, &kind.to_le_bytes());
        for (field, value) in [
            (8, flags),
            (16, if flags == 0 { 0 } else { BASE + offset }),
            (24, offset),
            (32, size),
            (48, 1),
        ] {
            put(&mut data, at + field, &value.to_le_bytes());
        }
    }
    data
}

fn check_parsed_elf(image: &Image<'_>) {
    use crate::MemoryTrait;

    assert!(matches!(image.image_type, ImageType::ElfImage(_)));
    assert_eq!(
        image.get_root_functions().unwrap(),
        (0..6u64)
            .map(|i| {
                let start = TEXT + i * 0x40;
                start..start + 0x40
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        run(image, string_matcher(NARROW_TEXT, false).xrefs().xrefs()).unwrap(),
        [SHARED_CALLER, OTHER_CALLER]
    );
    assert_eq!(
        run(image, string_matcher(WIDE_TEXT, true).xrefs().xrefs()).unwrap(),
        [SHARED_CALLER]
    );
    let matcher = common_string_caller(NARROW_TEXT, WIDE_TEXT)
        .xrefs()
        .addresses()
        .with_context()
        .modify_with(|(ctx, address)| {
            if ctx.image().memory.u8(address + 8)? != 0xe8 {
                bail_out!("expected call opcode");
            }
            Ok(address + 8)
        })
        .sub(|addresses| {
            assert_eq!(addresses, [ENTRY + 8]);
            Ok(BinaryMatcher::address(addresses[0]).offset(-8))
        });
    assert_eq!(run(image, matcher).unwrap(), [ENTRY]);
}

#[test]
fn parsed_elf_file_discovers_functions_and_resolves_nested_matchers() {
    let bytes = elf_file();
    let image = Image::builder().functions(true).build(&bytes).unwrap();
    check_parsed_elf(&image);
}

#[test]
fn mapped_elf_discovers_functions_from_program_headers_and_resolves_nested_matchers() {
    let bytes = elf_file();
    let image = Image::read::<&str>(Some(0), &bytes, None, true).unwrap();
    check_parsed_elf(&image);
}
