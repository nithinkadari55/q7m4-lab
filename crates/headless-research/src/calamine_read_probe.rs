//! Read-only, marked synthetic XLSX diagnostic; not a production upload boundary.
//! Calamine0.37.0: metadata streaming first, conservative bounded dense comparison.
mod calamine_guard;

use calamine::{Data, Reader, Xlsx, XlsxFormulaMetadata};
use calamine_guard::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fmt::Write as _, io::Cursor, path::Path};

const MAX_COORDINATES: usize = 32;
const MAX_SCAN_RECORDS: usize = 100_000;
const MAX_DENSE_CELLS: u64 = 65_536;
const MAX_SHARED_INDEX: usize = 4095;
const MAX_SHARED_TEMPLATES: usize = 1024;
const MAX_FORMULA_BYTES: usize = 4096;
const MAX_EXPANDED_FORMULA_BYTES: u64 = 1024 * 1024;
const MAX_REPORT_BYTES: usize = 96 * 1024;

fn text_fact(text: &str) -> Value {
    let mut end = text.len().min(1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"text":&text[..end],"source_utf8_bytes":text.len(),"truncated":end!=text.len()})
}

fn typed_data(data: &Data, epoch_1904: bool) -> Value {
    match data {
        Data::Int(value) => json!({"variant":"Int","i64_decimal":value.to_string()}),
        Data::Float(value) => json!({"variant":"Float","finite_value":value.is_finite().then_some(value),
            "ieee754_bits_hex":format!("{:016x}",value.to_bits())}),
        Data::String(value) => json!({"variant":"String","content":text_fact(value)}),
        Data::Bool(value) => json!({"variant":"Bool","value":value}),
        Data::Error(value) => json!({"variant":"Error","symbol":value.to_string(),"enum_debug":format!("{value:?}")}),
        Data::Empty => json!({"variant":"Empty"}),
        Data::DateTime(value) => json!({"variant":"DateTime","serial":value.as_f64(),"epoch_1904":epoch_1904,
            "is_duration":value.is_duration(),"is_datetime":value.is_datetime(),"raw_debug":format!("{value:?}")}),
        Data::DateTimeIso(value) => json!({"variant":"DateTimeIso","content":text_fact(value)}),
        Data::DurationIso(value) => json!({"variant":"DurationIso","content":text_fact(value)}),
    }
}

fn formula_metadata(metadata: Option<&XlsxFormulaMetadata>) -> Value {
    match metadata {
        None => json!({"kind":"none_reported"}),
        Some(XlsxFormulaMetadata::Normal { formula }) => json!({"kind":"normal","formula":text_fact(formula)}),
        Some(XlsxFormulaMetadata::Shared { shared_index, range, formula }) => json!({
            "kind":"shared_master","shared_index":shared_index,"formula":text_fact(formula),
            "range_zero_based":range.as_ref().map(|range|json!({"start":range.start,"end":range.end}))}),
        Some(XlsxFormulaMetadata::SharedDerived { shared_index }) => json!({
            "kind":"shared_derived","shared_index":shared_index,"expanded_text_not_requested_in_metadata_pass":true}),
        Some(other) => json!({"kind":"unknown_variant","debug":text_fact(&format!("{other:?}"))}),
    }
}

fn inspect(source: &[u8], sheet_name: &str, addresses: &[String]) -> ProbeResult<Value> {
    if sheet_name.len() > 128 || addresses.is_empty() || addresses.len() > MAX_COORDINATES {
        return Err(invalid("sheet/coordinate request cap exceeded"));
    }
    let positions: Vec<(u32, u32)> = addresses.iter().map(|address| {
        coordinate(address).map(|(column, row)| (row - 1, column - 1))
    }).collect::<ProbeResult<_>>()?;
    let parts = inventory(source, true)?;
    let expanded_bytes: u64 = parts.values().map(|part| part.bytes).sum();
    let mut inventory_description=String::new();
    for (name,part) in &parts {
        writeln!(&mut inventory_description,"{name}:{}",part.sha256).expect("format inventory into String");
    }
    let inventory_hash=hash(inventory_description.as_bytes());
    let mut book = Xlsx::new(Cursor::new(source))?;
    let sheet_count = book.sheet_names().len();
    if sheet_count > 128 {
        return Err(invalid("diagnostic workbook sheet-count cap exceeded"));
    }
    let epoch_1904 = book.has_1904_epoch();
    let defined_name_count = book.defined_names().len();
    let mut selected: BTreeMap<(u32,u32),(Data,Value)> = BTreeMap::new();
    let mut records = 0usize;
    let mut bounds: Option<((u32,u32),(u32,u32))> = None;
    let mut shared_lengths: BTreeMap<usize, usize> = BTreeMap::new();
    let mut expanded_formula_bytes = 0u64;
    let mut dense_refusal: Option<&str> = None;
    let declared_dimension;
    {
        let mut reader = book.worksheet_cells_reader(sheet_name)?;
        let declared = reader.dimensions();
        declared_dimension = json!({"start":declared.start,"end":declared.end});
        while let Some(record) = reader.next_cell_with_formula_metadata()? {
            records += 1;
            if records > MAX_SCAN_RECORDS || record.pos.0 >= 1_048_576 || record.pos.1 >= 16_384 {
                return Err(invalid("stream record or coordinate bound exceeded"));
            }
            match &mut bounds {
                Some((start,end)) => {
                    start.0=start.0.min(record.pos.0); start.1=start.1.min(record.pos.1);
                    end.0=end.0.max(record.pos.0); end.1=end.1.max(record.pos.1);
                },
                None => bounds=Some((record.pos,record.pos)),
            }
            let metadata = record.formula;
            let length = match metadata.as_ref() {
                Some(XlsxFormulaMetadata::Normal { formula }) => Some(formula.len()),
                Some(XlsxFormulaMetadata::Shared { shared_index, formula, .. }) => {
                    if *shared_index > MAX_SHARED_INDEX || shared_lengths.len() >= MAX_SHARED_TEMPLATES {
                        dense_refusal.get_or_insert("shared-index/template cap");
                    } else {
                        shared_lengths.insert(*shared_index,formula.len());
                    }
                    Some(formula.len())
                },
                Some(XlsxFormulaMetadata::SharedDerived { shared_index }) => {
                    if *shared_index > MAX_SHARED_INDEX {
                        dense_refusal.get_or_insert("shared-index cap");
                    }
                    let length=shared_lengths.get(shared_index).copied();
                    if length.is_none() {
                        dense_refusal.get_or_insert("shared master not seen in stream order");
                    }
                    length
                },
                Some(_) => { dense_refusal.get_or_insert("unknown formula metadata"); None },
                None => None,
            };
            if let Some(length) = length {
                if length > MAX_FORMULA_BYTES {
                    dense_refusal.get_or_insert("formula text cap");
                }
                // Add a conservative per-reference growth allowance for bounded
                // A1 offsets; this is a diagnostic estimate, not an RSS proof.
                expanded_formula_bytes=expanded_formula_bytes.saturating_add(length as u64 * 4 + 32);
                if expanded_formula_bytes > MAX_EXPANDED_FORMULA_BYTES {
                    dense_refusal.get_or_insert("estimated expanded formula-text cap");
                }
            }
            if positions.contains(&record.pos) {
                let data: Data=record.value.into();
                if selected.insert(record.pos,(data,formula_metadata(metadata.as_ref()))).is_some() {
                    return Err(invalid("duplicate selected coordinate record"));
                }
            }
        }
    }
    let area=bounds.map(|(start,end)|u64::from(end.0-start.0+1)*u64::from(end.1-start.1+1)).unwrap_or(0);
    if area > MAX_DENSE_CELLS {
        dense_refusal.get_or_insert("actual stored-record bounding rectangle exceeds dense-cell cap");
    }
    let mut dense_values = None;
    let mut dense_formulas = None;
    if dense_refusal.is_none() {
        dense_values=Some(book.worksheet_range(sheet_name)?);
        dense_formulas=Some(book.worksheet_formula(sheet_name)?);
    }
    let observations: Vec<Value> = addresses.iter().zip(positions.iter()).map(|(address,position)| {
        let stored=selected.get(position);
        let dense_value=dense_values.as_ref().and_then(|range|range.get_value(*position));
        let dense_formula=dense_formulas.as_ref().and_then(|range|range.get_value(*position));
        json!({"coordinate":address,"position_zero_based_row_column":position,
               "stored_record_seen":stored.is_some(),
               "stream_cached_or_literal":stored.map(|(data,_)|typed_data(data,epoch_1904)),
               "storage_formula_metadata":stored.map(|(_,metadata)|metadata),
               "dense_cached_or_literal":dense_value.map(|data|typed_data(data,epoch_1904)),
               "worksheet_formula_text":dense_formula.map(|text|text_fact(text)),
               "dense_apis_called":dense_refusal.is_none(),
               "cache_freshness":"unverified_no_calculation_performed"})
    }).collect();
    let report=json!({
        "component":"calamine_read_only_xlsx","expected_version":"0.37.0",
        "expected_source_commit":"f2c5948242fdfe390e292313a1f87f9db19b9633",
        "expected_archive_sha256":"c9ca341b94bc4b29e08970b1a03914fad2dfe177e2ecb67fcd6de9046004aafb",
        "diagnostic_completed":true,"production_adoption_verified":false,
        "source_sha256":hash(source),"part_count":parts.len(),"expanded_package_bytes":expanded_bytes,
        "part_inventory_sha256":inventory_hash,"sheet":sheet_name,"sheet_count":sheet_count,
        "defined_name_count":defined_name_count,"epoch_1904":epoch_1904,
        "metadata_scan":{"records":records,"declared_dimension":declared_dimension,
            "actual_record_bounds_zero_based":bounds,"actual_dense_rectangle_cells":area,
            "estimated_formula_text_bytes":expanded_formula_bytes},
        "dense_comparison":{"called":dense_refusal.is_none(),"refusal_reason":dense_refusal},
        "cells":observations,"calculated_by_probe":false,"complete_dependency_graph":false,
        "limits":{"input_bytes":INPUT_CAP,"expanded_package_bytes":EXPANDED_CAP,"records":MAX_SCAN_RECORDS,
            "selected_coordinates":MAX_COORDINATES,"dense_cells":MAX_DENSE_CELLS,"shared_index":MAX_SHARED_INDEX,
            "formula_source_bytes":MAX_FORMULA_BYTES,"estimated_formula_text_bytes":MAX_EXPANDED_FORMULA_BYTES},
        "limitations":[
            "Diagnostic caps are not hostile-input or total CPU/RSS/wall-time guarantees; constructor/SST reserve and parser allocations remain separately unverified",
            "Metadata API preserves shared placement; it does not expose all array/data-table/spill metadata or establish semantic resolution",
            "worksheet_formula expands shared text through the candidate helper; independent native comparison remains necessary",
            "A missing record, an explicit empty record and an empty formula string are separate observations",
            "Cached Data/date formatting classification is not calculated truth, original serial XML typing, or display fidelity",
            "Only selected marked synthetic XLSX files are admitted; no XLS/XLSB/ODS macro/render/editor support is established"
        ]
    });
    if serde_json::to_vec(&report)?.len() > MAX_REPORT_BYTES {
        return Err(invalid("diagnostic JSON report cap exceeded"));
    }
    Ok(report)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(3..=MAX_COORDINATES + 2).contains(&args.len()) {
        eprintln!("usage: calamine-read-probe INPUT.xlsx SHEET COORDINATE [COORDINATE ...] (maximum32)");
        std::process::exit(2);
    }
    let result = (|| -> ProbeResult<Value> {
        let path=Path::new(&args[0]);
        let source=read_bounded(path,INPUT_CAP)?;
        let mut report=inspect(&source,&args[1],&args[2..])?;
        report["original_unchanged"]=json!(read_bounded(path,INPUT_CAP)?==source);
        if serde_json::to_vec(&report)?.len()>MAX_REPORT_BYTES {
            return Err(invalid("final diagnostic JSON report cap exceeded"));
        }
        Ok(report)
    })();
    match result {
        Ok(report) => {
            println!("{report:#}");
            if report.get("original_unchanged").and_then(Value::as_bool)!=Some(true) {
                std::process::exit(1);
            }
        },
        Err(error) => {
            println!("{}",json!({"diagnostic_completed":false,"probe_error":error.to_string()}));
            std::process::exit(1);
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{ZipWriter,write::SimpleFileOptions};

    fn fixture(sheet_xml: &str) -> Vec<u8> {
        let mut writer=ZipWriter::new(Cursor::new(Vec::new()));
        let options=SimpleFileOptions::default().compression_method(zip::CompressionMethod::DEFLATE);
        let parts=[
            ("[Content_Types].xml",r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="txt" ContentType="text/plain"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#),
            ("_rels/.rels",r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
            ("xl/workbook.xml",r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><workbookPr date1904="0"/><sheets><sheet name="Fixture" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
            ("xl/_rels/workbook.xml.rels",r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#),
            ("xl/styles.xml",r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="14" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/></cellXfs></styleSheet>"#),
            ("xl/worksheets/sheet1.xml",sheet_xml),
            ("probe/fixture-id.txt","headless-excel-editor-synthetic-v1\n"),
        ];
        for (name,text) in parts {
            writer.start_file(name,options).expect("start synthetic part");
            writer.write_all(text.as_bytes()).expect("write synthetic part");
        }
        writer.finish().expect("finish synthetic package").into_inner()
    }

    #[test]
    fn raw_cached_types_remain_distinct_from_formula_and_empty_record() {
        let bytes=fixture(r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:H1"/><sheetData><row r="1"><c r="A1" t="e"><v>#N/A</v></c><c r="B1" t="n"><v>2042</v></c><c r="C1" t="inlineStr"><is><t>002042</t></is></c><c r="D1" t="b"><v>1</v></c><c r="E1" s="1"><v>45292</v></c><c r="F1"/><c r="G1" t="str"><f>""</f><v></v></c><c r="H1"><f>A1</f><v>42</v></c></row></sheetData></worksheet>"#);
        let addresses=["A1","B1","C1","D1","E1","F1","G1","H1","Z1"].map(str::to_string);
        let report=inspect(&bytes,"Fixture",&addresses).expect("inspect tiny typed fixture");
        let rows=report["cells"].as_array().expect("selected cells");
        assert_eq!(rows[0]["stream_cached_or_literal"]["variant"],"Error");
        assert_eq!(rows[0]["stream_cached_or_literal"]["symbol"],"#N/A");
        assert_eq!(rows[1]["stream_cached_or_literal"]["variant"],"Float");
        assert_eq!(rows[1]["stream_cached_or_literal"]["finite_value"],2042.0);
        assert_eq!(rows[2]["stream_cached_or_literal"]["content"]["text"],"002042");
        assert_eq!(rows[3]["stream_cached_or_literal"]["variant"],"Bool");
        assert_eq!(rows[4]["stream_cached_or_literal"]["variant"],"DateTime");
        assert_eq!(rows[4]["stream_cached_or_literal"]["serial"],45292.0);
        assert_eq!(rows[5]["stored_record_seen"],true);
        assert_eq!(rows[5]["stream_cached_or_literal"]["variant"],"Empty");
        assert_eq!(rows[6]["stream_cached_or_literal"]["variant"],"String");
        assert_eq!(rows[6]["stream_cached_or_literal"]["content"]["text"],"");
        assert_eq!(rows[6]["worksheet_formula_text"]["text"],"\"\"");
        assert_eq!(rows[7]["stream_cached_or_literal"]["finite_value"],42.0);
        assert_eq!(rows[7]["worksheet_formula_text"]["text"],"A1");
        assert_eq!(rows[8]["stored_record_seen"],false);
        assert_eq!(report["calculated_by_probe"],false);
    }

    #[test]
    fn sparse_metadata_is_retained_when_host_refuses_dense_rectangle() {
        // Two stored values; the host refuses the132k-cell dense representation.
        // The candidate dense constructor is never invoked on this case.
        let bytes=fixture(r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:CV1320"/><sheetData><row r="1"><c r="A1"><v>7</v></c></row><row r="1320"><c r="CV1320"><v>11</v></c></row></sheetData></worksheet>"#);
        let report=inspect(&bytes,"Fixture",&["A1".into(),"CV1320".into()]).expect("bounded metadata scan");
        assert_eq!(report["metadata_scan"]["records"],2);
        assert_eq!(report["metadata_scan"]["actual_dense_rectangle_cells"],132000);
        assert_eq!(report["dense_comparison"]["called"],false);
        assert_eq!(report["cells"][0]["stream_cached_or_literal"]["finite_value"],7.0);
        assert_eq!(report["cells"][1]["stream_cached_or_literal"]["finite_value"],11.0);
    }
}
