use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

mod tui;

/// Archive/combine PDF pages into a new PDF file.
#[derive(Parser, Debug)]
#[command(
    name = "pdf_archiver",
    version,
    about = "Extract and combine PDF pages"
)]
struct Cli {
    /// Input specifications in the form `file.pdf` or `file.pdf:1,3-5,7`.
    /// Use `z` (or `last`/`end`) for the last page, e.g. `file.pdf:10-z`.
    /// Open ranges are also accepted: `file.pdf:10-` (to the end) and `file.pdf:-3`.
    /// Repeating an input or page keeps it again (order is preserved).
    /// Omit to launch the interactive TUI.
    #[arg(value_name = "INPUT[:PAGES]")]
    inputs: Vec<String>,

    /// Force interactive TUI mode.
    #[arg(long = "tui")]
    tui: bool,

    /// Output PDF path (CLI mode). Defaults to output.pdf when omitted.
    #[arg(short, long, value_name = "OUTPUT")]
    output: Option<PathBuf>,

    /// Overwrite the output file without prompting.
    #[arg(short = 'y', long = "yes")]
    force: bool,
}

fn parse_page_token(token: &str, total: u32) -> Result<u32> {
    let token = token.trim();
    // `z` (and a few common aliases) stands for the last page of the document.
    if matches!(
        token.to_ascii_lowercase().as_str(),
        "z" | "last" | "end" | "$"
    ) {
        return Ok(total);
    }
    let page: u32 = token
        .parse()
        .with_context(|| format!("invalid page number: {token:?}"))?;
    if page == 0 {
        bail!("page numbers are 1-based, got 0");
    }
    if page > total {
        bail!("page out of range: requested {page}, document has {total} pages");
    }
    Ok(page)
}

fn parse_pages(spec: &str, total: u32) -> Result<Vec<u32>> {
    let mut result = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            // Allow open-ended ranges such as "10-" (to the end) or "-3" (from the start).
            let start: u32 = if start.trim().is_empty() {
                1
            } else {
                parse_page_token(start, total)?
            };
            let end: u32 = if end.trim().is_empty() {
                total
            } else {
                parse_page_token(end, total)?
            };
            if start == 0 || end == 0 {
                bail!("page numbers are 1-based, got 0");
            }
            if start > total || end > total {
                bail!("page out of range: requested {start}-{end}, document has {total} pages");
            }
            let (from, to) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            for page in from..=to {
                result.push(page);
            }
        } else {
            let page = parse_page_token(part, total)?;
            if page == 0 {
                bail!("page numbers are 1-based, got 0");
            }
            if page > total {
                bail!("page out of range: requested {page}, document has {total} pages");
            }
            result.push(page);
        }
    }
    Ok(result)
}

fn split_input(raw: &str) -> (String, Option<String>) {
    if let Some((path, pages)) = raw.rsplit_once(':')
        && !pages.contains('/')
        && !pages.contains('\\')
        && !path.is_empty()
    {
        return (path.to_string(), Some(pages.to_string()));
    }
    (raw.to_string(), None)
}

fn inherited_value(src: &Document, start: ObjectId, key: &[u8]) -> Option<Object> {
    let mut current = start;
    loop {
        let dict = src.get_object(current).ok()?.as_dict().ok()?;
        if let Ok(val) = dict.get(key) {
            return Some(val.clone());
        }
        {
            let parent = dict
                .get(b"Parent")
                .ok()
                .and_then(|o| o.as_reference().ok())?;
            current = parent
        }
    }
}

fn remap_object(
    src: &Document,
    dst: &mut Document,
    obj: &Object,
    mapping: &mut BTreeMap<ObjectId, ObjectId>,
    pages_id: ObjectId,
) -> Result<Object> {
    match obj {
        Object::Reference(id) => {
            let new = copy_object(src, dst, *id, mapping, pages_id)?;
            Ok(Object::Reference(new))
        }
        Object::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(remap_object(src, dst, item, mapping, pages_id)?);
            }
            Ok(Object::Array(out))
        }
        Object::Dictionary(dict) => {
            let mut out = Dictionary::new();
            for (k, v) in dict.iter() {
                out.set(k.clone(), remap_object(src, dst, v, mapping, pages_id)?);
            }
            Ok(Object::Dictionary(out))
        }
        Object::Stream(stream) => {
            let mut out_stream = stream.clone();
            let keys: Vec<Vec<u8>> = out_stream.dict.iter().map(|(k, _)| k.clone()).collect();
            for key in keys {
                if let Ok(val) = out_stream.dict.get(&key) {
                    let val = val.clone();
                    out_stream
                        .dict
                        .set(key, remap_object(src, dst, &val, mapping, pages_id)?);
                }
            }
            Ok(Object::Stream(out_stream))
        }
        other => Ok(other.clone()),
    }
}

fn copy_object(
    src: &Document,
    dst: &mut Document,
    id: ObjectId,
    mapping: &mut BTreeMap<ObjectId, ObjectId>,
    pages_id: ObjectId,
) -> Result<ObjectId> {
    if let Some(&new_id) = mapping.get(&id) {
        return Ok(new_id);
    }
    let object = src.get_object(id)?.clone();
    let new_id = dst.new_object_id();
    mapping.insert(id, new_id);

    let is_page = match &object {
        Object::Dictionary(d) => d
            .get(b"Type")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| n == b"Page")
            .unwrap_or(false),
        _ => false,
    };

    let new_object = match object {
        Object::Dictionary(mut dict) => {
            let keys: Vec<Vec<u8>> = dict.iter().map(|(k, _)| k.clone()).collect();
            for key in keys {
                if is_page && key == b"Parent" {
                    dict.set(key, Object::Reference(pages_id));
                    continue;
                }
                if let Ok(val) = dict.get(&key) {
                    let val = val.clone();
                    let new_val = remap_object(src, dst, &val, mapping, pages_id)?;
                    dict.set(key, new_val);
                }
            }
            Object::Dictionary(dict)
        }
        Object::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(remap_object(src, dst, &item, mapping, pages_id)?);
            }
            Object::Array(out)
        }
        Object::Stream(mut stream) => {
            let keys: Vec<Vec<u8>> = stream.dict.iter().map(|(k, _)| k.clone()).collect();
            for key in keys {
                if let Ok(val) = stream.dict.get(&key) {
                    let val = val.clone();
                    stream
                        .dict
                        .set(key, remap_object(src, dst, &val, mapping, pages_id)?);
                }
            }
            Object::Stream(stream)
        }
        other => other,
    };

    dst.set_object(new_id, new_object);
    Ok(new_id)
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    if cli.tui || cli.inputs.is_empty() {
        return tui::run_tui();
    }
    if cli.output.is_none() {
        cli.output = Some(PathBuf::from("output.pdf"));
    }
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    let output = cli
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("output.pdf"));
    // First pass: validate inputs and page ranges.
    let mut resolved: Vec<(PathBuf, Option<Vec<u32>>)> = Vec::new();
    let mut raw_inputs: Vec<(PathBuf, Option<String>)> = Vec::with_capacity(cli.inputs.len());
    for raw in &cli.inputs {
        let (path, pages) = split_input(raw);
        raw_inputs.push((PathBuf::from(path), pages));
    }

    for (path, selector) in &raw_inputs {
        let doc = Document::load(path)
            .with_context(|| format!("failed to load PDF: {}", path.display()))?;
        let total = doc.get_pages().len() as u32;
        let pages = match selector {
            Some(sel) => Some(parse_pages(sel, total)?),
            None => None,
        };
        resolved.push((path.clone(), pages));
    }

    if output.exists() && !cli.force {
        print!(
            "Output file {} already exists. Overwrite? [y/N] ",
            output.display()
        );
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("aborted: output file already exists");
        }
    }

    // Build the output document.
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let catalog_id = doc.add_object(Object::Dictionary({
        let mut d = Dictionary::new();
        d.set("Type", Object::Name(b"Catalog".to_vec()));
        d.set("Pages", Object::Reference(pages_id));
        d
    }));
    doc.trailer.set("Root", catalog_id);
    // placeholder pages; filled later
    doc.set_object(
        pages_id,
        Object::Dictionary({
            let mut d = Dictionary::new();
            d.set("Type", Object::Name(b"Pages".to_vec()));
            d.set("Kids", Object::Array(vec![]));
            d.set("Count", Object::Integer(0));
            d
        }),
    );

    let mut kids: Vec<Object> = Vec::new();

    for (path, pages) in &resolved {
        let src = Document::load(path)
            .with_context(|| format!("failed to load PDF: {}", path.display()))?;
        let total = src.get_pages().len() as u32;
        let src_pages = src.get_pages();
        let selected = match pages {
            Some(p) => p.clone(),
            None => (1..=total).collect(),
        };
        if selected.is_empty() {
            continue;
        }
        let mut mapping: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();

        for &page_no in &selected {
            let src_page_id = src_pages
                .get(&page_no)
                .copied()
                .ok_or_else(|| anyhow!("page {page_no} not found in {}", path.display()))?;

            let new_page_id = copy_object(&src, &mut doc, src_page_id, &mut mapping, pages_id)?;

            let has_media = doc
                .get_object(new_page_id)
                .ok()
                .and_then(|o| o.as_dict().ok())
                .map(|d| d.has(b"MediaBox"))
                .unwrap_or(false);
            let has_resources = doc
                .get_object(new_page_id)
                .ok()
                .and_then(|o| o.as_dict().ok())
                .map(|d| d.has(b"Resources"))
                .unwrap_or(false);

            let media_val = if !has_media {
                inherited_value(&src, src_page_id, b"MediaBox")
            } else {
                None
            };
            let resources_val = if !has_resources {
                inherited_value(&src, src_page_id, b"Resources")
            } else {
                None
            };

            let media_remapped = match media_val {
                Some(val) => Some(remap_object(&src, &mut doc, &val, &mut mapping, pages_id)?),
                None => None,
            };
            let resources_remapped = match resources_val {
                Some(val) => Some(remap_object(&src, &mut doc, &val, &mut mapping, pages_id)?),
                None => None,
            };

            if let Ok(page_dict) = doc.get_object_mut(new_page_id)?.as_dict_mut() {
                page_dict.set("Parent", Object::Reference(pages_id));
                if let Some(v) = media_remapped {
                    page_dict.set("MediaBox", v);
                }
                if let Some(v) = resources_remapped {
                    page_dict.set("Resources", v);
                }
            }

            kids.push(Object::Reference(new_page_id));
        }
    }

    if kids.is_empty() {
        bail!("no pages selected; nothing to write");
    }

    // Update pages tree.
    if let Ok(pages_dict) = doc.get_object_mut(pages_id)?.as_dict_mut() {
        pages_dict.set("Kids", Object::Array(kids.clone()));
        pages_dict.set("Count", Object::Integer(kids.len() as i64));
    }

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).ok();
    }
    doc.save(&output)
        .with_context(|| format!("failed to save output: {}", output.display()))?;

    println!("Wrote {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_pdf(path: &std::path::Path, pages: usize) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut kids = Vec::new();
        for _ in 0..pages {
            let page_id = doc.add_object(Object::Dictionary({
                let mut d = Dictionary::new();
                d.set("Type", Object::Name(b"Page".to_vec()));
                d.set("Parent", Object::Reference(pages_id));
                d.set(
                    "MediaBox",
                    Object::Array(vec![
                        Object::Integer(0),
                        Object::Integer(0),
                        Object::Integer(595),
                        Object::Integer(842),
                    ]),
                );
                d
            }));
            kids.push(Object::Reference(page_id));
        }
        let pages_dict = Object::Dictionary({
            let mut d = Dictionary::new();
            d.set("Type", Object::Name(b"Pages".to_vec()));
            d.set("Kids", Object::Array(kids));
            d.set("Count", Object::Integer(pages as i64));
            d
        });
        doc.objects.insert(pages_id, pages_dict);
        let catalog_id = doc.add_object(Object::Dictionary({
            let mut d = Dictionary::new();
            d.set("Type", Object::Name(b"Catalog".to_vec()));
            d.set("Pages", Object::Reference(pages_id));
            d
        }));
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    #[test]
    fn end_to_end_combine() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.pdf");
        let b = dir.path().join("b.pdf");
        let out = dir.path().join("out.pdf");

        make_pdf(&a, 3);
        make_pdf(&b, 2);

        let cli = Cli {
            inputs: vec![
                format!("{}:2-3", a.display()),
                format!("{}:1,2", b.display()),
            ],
            output: Some(out.clone()),
            force: true,
            tui: false,
        };
        run(cli).unwrap();
        assert!(out.exists());
        let doc = Document::load(&out).unwrap();
        assert_eq!(doc.get_pages().len(), 4);
    }

    #[test]
    fn parse_pages_basic() {
        assert_eq!(parse_pages("1,3-5,7", 10).unwrap(), vec![1, 3, 4, 5, 7]);
    }

    #[test]
    fn parse_pages_reversed_range() {
        assert_eq!(parse_pages("5-3", 10).unwrap(), vec![3, 4, 5]);
    }

    #[test]
    fn parse_pages_last_symbol() {
        assert_eq!(
            parse_pages("10-z", 12).unwrap(),
            (10..=12).collect::<Vec<_>>()
        );
        assert_eq!(parse_pages("z", 5).unwrap(), vec![5]);
        assert_eq!(parse_pages("5-z", 5).unwrap(), vec![5]);
        assert_eq!(
            parse_pages("10-", 12).unwrap(),
            (10..=12).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_pages_duplicates_allowed() {
        assert_eq!(parse_pages("1,1,2", 3).unwrap(), vec![1, 1, 2]);
    }

    #[test]
    fn parse_pages_out_of_range() {
        assert!(parse_pages("11", 3).is_err());
    }

    #[test]
    fn parse_pages_zero_rejected() {
        assert!(parse_pages("0", 3).is_err());
    }

    #[test]
    fn split_input_with_pages() {
        assert_eq!(
            split_input("foo.pdf:1-3,5"),
            ("foo.pdf".to_string(), Some("1-3,5".to_string()))
        );
    }

    #[test]
    fn split_input_without_pages() {
        assert_eq!(split_input("foo.pdf"), ("foo.pdf".to_string(), None));
    }
}
