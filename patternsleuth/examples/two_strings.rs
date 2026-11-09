use std::collections::HashSet;

use anyhow::{Result, ensure};
use patternsleuth::{
    binary_matchers::BinaryMatcher, image::Image, resolvers::eval, scanner::Pattern,
};

fn string_references<'scope>(value: &str) -> Result<BinaryMatcher<'scope>> {
    // Match a complete, null-terminated UTF-8 literal, not a longer prefix.
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    Ok(
        BinaryMatcher::pattern(Pattern::from_bytes(bytes.try_into()?))
            .xrefs()
            .addresses(),
    )
}

pub fn methods_referencing_both(image: &Image<'_>, first: &str, second: &str) -> Result<Vec<u64>> {
    let first = string_references(first)?;
    let second = string_references(second)?;

    eval(image, move |ctx| {
        Box::pin(async move {
            let mut candidates = first.resolve(ctx).await?;
            if candidates.is_empty() {
                return Ok(candidates);
            }

            // XRefs returns containing function starts. Intersecting these sets finds
            // functions referencing both literals, not callers of those functions.
            let second_refs: HashSet<_> = second.resolve(ctx).await?.into_iter().collect();
            candidates.retain(|address| second_refs.contains(address));
            candidates.sort_unstable();
            candidates.dedup();
            Ok(candidates)
        })
    })
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: two_strings <binary> <first-string> <second-string>"
    );
    let bytes = std::fs::read(&args[1])?;
    let image = Image::builder().functions(true).build(&bytes)?;
    for address in methods_referencing_both(&image, &args[2], &args[3])? {
        println!("{address:#x}");
    }
    Ok(())
}
