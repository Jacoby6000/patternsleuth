use super::{BASE, TEXT};
use crate::{
    Memory, NamedMemorySection,
    image::{Image, ImageType},
};
use object::SectionKind;

fn image(text: Vec<u8>, image_type: ImageType) -> Image<'static> {
    Image {
        base_address: BASE,
        memory: Memory {
            sections: vec![NamedMemorySection::new(
                ".text".into(),
                TEXT,
                SectionKind::Text,
                text,
            )],
        },
        #[cfg(feature = "symbols")]
        symbols: None,
        imports: Default::default(),
        image_type,
    }
}

#[cfg(feature = "image-pe")]
pub(super) fn pe(text: Vec<u8>, functions: u32) -> Image<'static> {
    let mut image = image(
        text,
        ImageType::PEImage(crate::image::pe::PEImage {
            exception_directory_range: BASE + 0x2000..BASE + 0x2000 + u64::from(functions) * 12,
            exception_children_cache: Default::default(),
        }),
    );
    let pdata: Vec<u8> = (0..functions)
        .flat_map(|i| [0x1000 + i * 0x40, 0x1040 + i * 0x40, 0x3000 + i * 4])
        .flat_map(u32::to_le_bytes)
        .collect();
    image.memory.sections.extend([
        NamedMemorySection::new(
            ".pdata".into(),
            BASE + 0x2000,
            SectionKind::ReadOnlyData,
            pdata,
        ),
        NamedMemorySection::new(
            ".xdata".into(),
            BASE + 0x3000,
            SectionKind::ReadOnlyData,
            [1, 0, 0, 0].repeat(functions as usize),
        ),
    ]);
    image
}

#[cfg(feature = "image-elf")]
pub(super) fn elf(text: Vec<u8>, functions: u32) -> Image<'static> {
    image(
        text,
        ImageType::ElfImage(crate::image::elf::ElfImage {
            functions: Some(
                (0..functions)
                    .map(|i| {
                        let start = TEXT + u64::from(i) * 0x40;
                        start..start + 0x40
                    })
                    .collect(),
            ),
        }),
    )
}
