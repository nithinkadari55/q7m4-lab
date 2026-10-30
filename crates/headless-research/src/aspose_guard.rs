//! Generic guards extracted from the reviewed synthetic umya diagnostic.
//! No total-RSS, CPU, XML schema or full hostile-file guarantee is implied.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::File,
    io::{self, Cursor, Read},
    path::{Path, PathBuf},
};

pub(crate) type ProbeResult<T> = Result<T, Box<dyn std::error::Error>>;
pub(crate) const INPUT_CAP: u64 = 16 * 1024 * 1024;
pub(crate) const OUTPUT_CAP: u64 = 32 * 1024 * 1024;
pub(crate) const ENTRY_CAP: u64 = 16 * 1024 * 1024;
pub(crate) const EXPANDED_CAP: u64 = 64 * 1024 * 1024;
pub(crate) const ENTRY_COUNT_CAP: usize = 2048;
pub(crate) const FIXTURE_MARKER: &str = "probe/fixture-id.txt";
pub(crate) const FIXTURE_ID: &[u8] = b"headless-excel-editor-synthetic-v1\n";
pub(crate) const OPAQUE_PARTS: &[&str] = &[
    "customXml/item1.xml",
    "xl/opaque/workbook-marker.xml",
    "xl/opaque/edited-sheet-marker.xml",
    "xl/opaque/untouched-sheet-marker.xml",
];

pub(crate) fn invalid(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

pub(crate) fn read_bounded(path: &Path, cap: u64) -> ProbeResult<Vec<u8>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > cap {
        return Err(invalid("file exceeds diagnostic source/output size cap"));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(invalid("file grew beyond size cap during read"));
    }
    Ok(bytes)
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut encoded, "{byte:02x}").expect("format into String");
    }
    encoded
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    bytes: u64,
    sha256: String,
}

pub(crate) fn inventory(bytes: &[u8], require_fixture: bool) -> ProbeResult<BTreeMap<String, Part>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > ENTRY_COUNT_CAP {
        return Err(invalid("ZIP entry count cap exceeded"));
    }
    let mut total = 0u64;
    let mut actual_total = 0u64;
    let mut parts = BTreeMap::new();
    let mut marker_valid = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        if name.starts_with('/') || name.contains('\\')
            || name.split('/').any(|segment| segment == "..")
        {
            return Err(invalid("non-canonical package entry path"));
        }
        total = total.checked_add(entry.size()).ok_or_else(|| invalid("ZIP size sum overflow"))?;
        if entry.size() > ENTRY_CAP || total > EXPANDED_CAP {
            return Err(invalid("ZIP declared expansion cap exceeded"));
        }
        let mut data = Vec::new();
        let remaining = EXPANDED_CAP.checked_sub(actual_total)
            .ok_or_else(|| invalid("ZIP actual expansion accounting exceeded cap"))?;
        let allowance = ENTRY_CAP.min(remaining);
        entry.take(allowance + 1).read_to_end(&mut data)?;
        if data.len() as u64 > allowance {
            return Err(invalid("ZIP actual entry/aggregate expansion cap exceeded"));
        }
        actual_total = actual_total.checked_add(data.len() as u64)
            .ok_or_else(|| invalid("ZIP actual expansion sum overflow"))?;
        if name == FIXTURE_MARKER {
            marker_valid = data == FIXTURE_ID;
        }
        if parts.insert(name, Part { bytes: data.len() as u64, sha256: hash(&data) }).is_some() {
            return Err(invalid("duplicate ZIP entry name"));
        }
    }
    if require_fixture && !marker_valid {
        return Err(invalid("input is not the private synthetic fixture (marker absent/incorrect)"));
    }
    Ok(parts)
}

/// Validated bounded coordinates avoid passing unchecked strings into library
/// coordinate conversions. umya tuple order is (column, row), one-based.
pub(crate) fn coordinate(text: &str) -> ProbeResult<(u32, u32)> {
    let mut column = 0u32;
    let mut row = 0u32;
    let mut digits = false;
    for byte in text.bytes() {
        if byte.is_ascii_uppercase() && !digits {
            column = column.checked_mul(26)
                .and_then(|value| value.checked_add(u32::from(byte - b'A' + 1)))
                .ok_or_else(|| invalid("column overflow"))?;
        } else if byte.is_ascii_digit() {
            digits = true;
            row = row.checked_mul(10)
                .and_then(|value| value.checked_add(u32::from(byte - b'0')))
                .ok_or_else(|| invalid("row overflow"))?;
        } else {
            return Err(invalid("cell must be an uppercase single A1 coordinate"));
        }
    }
    if !(1..=16384).contains(&column) || !(1..=1048576).contains(&row) {
        return Err(invalid("cell outside XLSX coordinate bounds"));
    }
    Ok((column, row))
}

pub(crate) fn new_output_path(input: &Path, output: &Path) -> ProbeResult<PathBuf> {
    let name = output.file_name().ok_or_else(|| invalid("output must name a new file"))?;
    if output.extension().and_then(|value| value.to_str()) != Some("xlsx") {
        return Err(invalid("this diagnostic emits .xlsx only"));
    }
    let parent = output.parent().filter(|value| !value.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let resolved = parent.canonicalize()?.join(name);
    if resolved == input.canonicalize()? {
        return Err(invalid("output aliases the immutable input path"));
    }
    // create_new at actual open is the final guard against existing files,
    // hard links and symlinks; this existence check is not used as authorization.
    Ok(resolved)
}

