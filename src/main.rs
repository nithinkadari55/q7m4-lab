//! Disposable COM diagnostics. Never attaches to an existing workbook or instance.
//! Raw VARIANTs are deliberately wrapped: generated bindings do not own their payloads.
use serde_json::{Value, json};
use std::{mem::ManuallyDrop, ptr};
use windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        System::{Com::*, Ole::*, Variant::*},
        UI::WindowsAndMessaging::GetWindowThreadProcessId,
    },
    core::{BSTR, GUID, PCWSTR, w},
};

type ProbeResult<T> = Result<T, String>;

struct Apartment;
impl Apartment {
    fn new() -> ProbeResult<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }
            .map_err(|e| format!("STA initialization: {e}"))?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

// Never derive Clone: a bitwise copy would duplicate owning BSTR/dispatch/array payloads.
#[derive(Default)]
struct OwnedVariant(VARIANT);
impl Drop for OwnedVariant {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}
impl OwnedVariant {
    fn typed(vt: VARENUM) -> Self {
        let mut v = Self::default();
        unsafe {
            (*v.0.Anonymous.Anonymous).vt = vt;
        }
        v
    }
    fn number(n: f64) -> Self {
        let mut v = Self::typed(VT_R8);
        unsafe {
            (*v.0.Anonymous.Anonymous).Anonymous.dblVal = n;
        }
        v
    }
    fn boolean(b: bool) -> Self {
        let mut v = Self::typed(VT_BOOL);
        unsafe {
            (*v.0.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_BOOL(if b { -1 } else { 0 });
        }
        v
    }
    fn text(s: &str) -> Self {
        let mut v = Self::typed(VT_BSTR);
        unsafe {
            (*v.0.Anonymous.Anonymous).Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(s));
        }
        v
    }
    fn error(code: i32) -> Self {
        let mut v = Self::typed(VT_ERROR);
        unsafe {
            (*v.0.Anonymous.Anonymous).Anonymous.scode = code;
        }
        v
    }
    fn missing() -> Self {
        Self::error(0x80020004_u32 as i32)
    }
    fn vt(&self) -> VARENUM {
        unsafe { self.0.Anonymous.Anonymous.vt }
    }
    fn dispatch(&self) -> ProbeResult<IDispatch> {
        if self.vt() != VT_DISPATCH {
            return Err(format!("expected dispatch, got {}", self.vt().0));
        }
        unsafe {
            self.0
                .Anonymous
                .Anonymous
                .Anonymous
                .pdispVal
                .as_ref()
                .cloned()
        }
        .ok_or_else(|| "null dispatch".to_string())
    }
    fn scalar(&self) -> ProbeResult<Value> {
        unsafe {
            let a = &self.0.Anonymous.Anonymous.Anonymous;
            Ok(match self.vt() {
                VT_EMPTY => json!({"kind":"empty"}),
                VT_NULL => json!({"kind":"null"}),
                VT_R8 => json!({"kind":"number","value":a.dblVal}),
                VT_I4 => json!({"kind":"number","value":a.lVal}),
                VT_I2 => json!({"kind":"number","value":a.iVal}),
                VT_BOOL => json!({"kind":"boolean","value":a.boolVal.0 != 0}),
                VT_BSTR => json!({"kind":"text","value":a.bstrVal.to_string()}),
                VT_ERROR => json!({"kind":"error","code":a.scode}),
                vt => return Err(format!("unhandled variant {}", vt.0)),
            })
        }
    }
    fn array(rows: i32, cols: i32, row_base: i32, col_base: i32) -> ProbeResult<Self> {
        // Use API dimension 1 for rows and dimension 2 for columns.
        // SafeArrayCreate's input bounds follow the same order as its index API.
        let bounds = [
            SAFEARRAYBOUND {
                cElements: rows as u32,
                lLbound: row_base,
            },
            SAFEARRAYBOUND {
                cElements: cols as u32,
                lLbound: col_base,
            },
        ];
        let array = unsafe { SafeArrayCreate(VT_VARIANT, 2, bounds.as_ptr()) };
        if array.is_null() {
            return Err("SafeArrayCreate returned null".into());
        }
        let mut v = Self::typed(VT_ARRAY | VT_VARIANT);
        unsafe {
            (*v.0.Anonymous.Anonymous).Anonymous.parray = array;
        }
        Ok(v)
    }
    fn array_ptr(&self) -> ProbeResult<*mut SAFEARRAY> {
        if self.vt() != (VT_ARRAY | VT_VARIANT) {
            return Err(format!("expected variant array, got {}", self.vt().0));
        }
        Ok(unsafe { self.0.Anonymous.Anonymous.Anonymous.parray })
    }
    fn array_put(&mut self, row: i32, col: i32, value: &OwnedVariant) -> ProbeResult<()> {
        unsafe {
            SafeArrayPutElement(
                self.array_ptr()?,
                [row, col].as_ptr(),
                ptr::from_ref(&value.0).cast(),
            )
        }
        .map_err(|e| format!("SafeArrayPutElement: {e}"))
    }
    fn array_snapshot(&self) -> ProbeResult<Value> {
        let array = self.array_ptr()?;
        let mut bounds = Vec::new();
        for dim in 1..=2 {
            bounds.push(unsafe {
                (
                    SafeArrayGetLBound(array, dim).map_err(|e| e.to_string())?,
                    SafeArrayGetUBound(array, dim).map_err(|e| e.to_string())?,
                )
            });
        }
        let mut cells = Vec::new();
        for row in bounds[0].0..=bounds[0].1 {
            for col in bounds[1].0..=bounds[1].1 {
                let mut v = Self::default();
                unsafe {
                    SafeArrayGetElement(array, [row, col].as_ptr(), ptr::from_mut(&mut v.0).cast())
                }
                .map_err(|e| e.to_string())?;
                cells.push(v.scalar()?);
            }
        }
        Ok(json!({"bounds":bounds,"cells":cells}))
    }
}

fn invoke(
    object: &IDispatch,
    name: &str,
    flags: DISPATCH_FLAGS,
    mut args: Vec<OwnedVariant>,
) -> ProbeResult<OwnedVariant> {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut dispid = 0;
    unsafe { object.GetIDsOfNames(&GUID::zeroed(), &PCWSTR(wide.as_ptr()), 1, 0, &mut dispid) }
        .map_err(|e| format!("{name} lookup HRESULT={:08x}", e.code().0))?;
    args.reverse();
    // Move raw variants into a contiguous argument vector; ownership stays here until Invoke returns.
    let mut raw: Vec<VARIANT> = args
        .into_iter()
        .map(|mut v| std::mem::take(&mut v.0))
        .collect();
    let mut named = -3; // DISPID_PROPERTYPUT
    let put = flags == DISPATCH_PROPERTYPUT || flags == DISPATCH_PROPERTYPUTREF;
    let params = DISPPARAMS {
        rgvarg: raw.as_mut_ptr(),
        rgdispidNamedArgs: if put { &mut named } else { ptr::null_mut() },
        cArgs: raw.len() as u32,
        cNamedArgs: u32::from(put),
    };
    let mut output = OwnedVariant::default();
    let mut ex = EXCEPINFO::default();
    let mut arg_error = u32::MAX;
    let result = unsafe {
        object.Invoke(
            dispid,
            &GUID::zeroed(),
            0,
            flags,
            &params,
            Some(&mut output.0),
            Some(&mut ex),
            Some(&mut arg_error),
        )
    };
    if result.is_err()
        && let Some(fill) = ex.pfnDeferredFillIn
    {
        unsafe {
            let _ = fill(&mut ex);
        }
    }
    let error = result.err().map(|e| {
        format!(
            "{name} HRESULT={:08x} scode={:08x} reversed_arg={} source={} description={}",
            e.code().0,
            ex.scode,
            arg_error,
            *ex.bstrSource,
            *ex.bstrDescription
        )
    });
    unsafe {
        for value in &mut raw {
            let _ = VariantClear(value);
        }
        ManuallyDrop::drop(&mut ex.bstrSource);
        ManuallyDrop::drop(&mut ex.bstrDescription);
        ManuallyDrop::drop(&mut ex.bstrHelpFile);
    }
    match error {
        Some(e) => Err(e),
        None => Ok(output),
    }
}
fn get(object: &IDispatch, name: &str) -> ProbeResult<OwnedVariant> {
    invoke(object, name, DISPATCH_PROPERTYGET, vec![])
}
fn put(object: &IDispatch, name: &str, value: OwnedVariant) -> ProbeResult<()> {
    invoke(object, name, DISPATCH_PROPERTYPUT, vec![value]).map(drop)
}
fn check(report: &mut Vec<Value>, name: &str, pass: bool, detail: Value) -> ProbeResult<()> {
    report.push(json!({"name":name,"pass":pass,"detail":detail}));
    if pass {
        Ok(())
    } else {
        Err(format!("check failed: {name}"))
    }
}

struct OwnedExcel {
    app: IDispatch,
    book: Option<IDispatch>,
    closed: bool,
}
impl OwnedExcel {
    fn close(&mut self) -> ProbeResult<()> {
        if self.closed {
            return Ok(());
        }
        let book_result = self.book.take().map(|b| {
            invoke(
                &b,
                "Close",
                DISPATCH_METHOD,
                vec![OwnedVariant::boolean(false)],
            )
        });
        let quit = invoke(&self.app, "Quit", DISPATCH_METHOD, vec![]);
        self.closed = quit.is_ok();
        if let Some(r) = book_result {
            r?;
        }
        quit.map(drop)
    }
}
impl Drop for OwnedExcel {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn excel_checks(owner: &mut OwnedExcel, report: &mut Vec<Value>) -> ProbeResult<Value> {
    let app = &owner.app;
    put(app, "Visible", OwnedVariant::boolean(false))?;
    put(app, "DisplayAlerts", OwnedVariant::boolean(false))?;
    put(app, "AutomationSecurity", OwnedVariant::number(3.0))?;
    let environment = json!({"version":get(app,"Version")?.scalar()?,"build":get(app,"Build")?.scalar()?,"hwnd":get(app,"Hwnd")?.scalar()?});
    let hwnd_value = get(app, "Hwnd")?.scalar()?["value"]
        .as_i64()
        .ok_or("missing HWND")?;
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(
            windows::Win32::Foundation::HWND(hwnd_value as u32 as usize as *mut _),
            Some(&mut pid),
        );
    }
    check(
        report,
        "owned_instance_pid",
        pid != 0,
        json!({"pid":pid,"environment":environment}),
    )?;
    let books = get(app, "Workbooks")?.dispatch()?;
    let book = invoke(
        &books,
        "Add",
        DISPATCH_METHOD,
        vec![OwnedVariant::missing()],
    )?
    .dispatch()?;
    owner.book = Some(book.clone());
    let sheets = get(&book, "Worksheets")?.dispatch()?;
    let sheet = invoke(
        &sheets,
        "Item",
        DISPATCH_PROPERTYGET,
        vec![OwnedVariant::number(1.0)],
    )?
    .dispatch()?;
    put(&sheet, "Name", OwnedVariant::text("SyntheticProbe"))?;
    let range = invoke(
        &sheet,
        "Range",
        DISPATCH_PROPERTYGET,
        vec![OwnedVariant::text("A1"), OwnedVariant::text("C2")],
    )?
    .dispatch()?;
    let values = [
        OwnedVariant::text("Unicode \u{03a9} \u{4e2d}\u{6587}"),
        OwnedVariant::boolean(true),
        OwnedVariant::number(42.125),
        OwnedVariant::default(),
        OwnedVariant::error(-2146826281),
        OwnedVariant::number(-2146826281.0),
    ];
    let mut array = OwnedVariant::array(2, 3, 1, 1)?;
    for (i, value) in values.iter().enumerate() {
        array.array_put((i / 3) as i32 + 1, (i % 3) as i32 + 1, value)?;
    }
    let expected = array.array_snapshot()?;
    put(&range, "Value2", array)?;
    let actual = get(&range, "Value2")?.array_snapshot()?;
    check(
        report,
        "mixed_2d_round_trip",
        actual == expected,
        json!({"expected":expected,"actual":actual}),
    )?;
    let formula = invoke(
        &sheet,
        "Range",
        DISPATCH_PROPERTYGET,
        vec![OwnedVariant::text("D1")],
    )?
    .dispatch()?;
    put(&formula, "Formula2", OwnedVariant::text("=SUM(C1,7)"))?;
    invoke(app, "CalculateFull", DISPATCH_METHOD, vec![])?;
    let result = get(&formula, "Value2")?.scalar()?;
    check(
        report,
        "native_excel_calculation",
        result["value"] == json!(49.125),
        result,
    )?;
    let bad_range = invoke(
        &sheet,
        "Range",
        DISPATCH_PROPERTYGET,
        vec![OwnedVariant::text("not a valid range!!")],
    )
    .err()
    .unwrap_or_default();
    check(
        report,
        "excel_exception_details",
        bad_range.contains("HRESULT=")
            && bad_range.contains("description=")
            && !bad_range.ends_with("description="),
        json!({"error":bad_range}),
    )?;
    let missing_member = get(app, "ThisMemberDoesNotExist_Probe")
        .err()
        .unwrap_or_default();
    check(
        report,
        "unknown_member_failure",
        missing_member.contains("80020006"),
        json!({"error":missing_member}),
    )?;
    // Repeated allocate/copy/read/clear exercises ownership, not a memory-leak proof.
    for _ in 0..200 {
        let value = get(&range, "Value2")?;
        value.array_snapshot()?;
    }
    check(
        report,
        "repeated_variant_cleanup",
        true,
        json!({"iterations":200,"leak_proof":false}),
    )?;
    Ok(json!({"pid":pid,"environment":environment}))
}

fn main() {
    let mut checks = Vec::new();
    let mut cleanup = json!({"owned_excel_quit":false});
    let outcome = (|| -> ProbeResult<Value> {
        let _sta = Apartment::new()?;
        let mut local = OwnedVariant::array(2, 3, 5, 7)?;
        local.array_put(5, 7, &OwnedVariant::text("offset"))?;
        let snapshot = local.array_snapshot()?;
        check(
            &mut checks,
            "nonzero_safearray_bounds",
            snapshot["bounds"] == json!([[5, 6], [7, 9]])
                && snapshot["cells"][0]["value"] == "offset",
            snapshot,
        )?;
        let clsid =
            unsafe { CLSIDFromProgID(w!("Excel.Application")) }.map_err(|e| e.to_string())?;
        let app: IDispatch = unsafe { CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|e| format!("create Excel: {e}"))?;
        let mut owner = OwnedExcel {
            app,
            book: None,
            closed: false,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            excel_checks(&mut owner, &mut checks)
        }));
        let close = owner.close();
        cleanup = json!({"owned_excel_quit":close.is_ok(),"error":close.err()});
        match result {
            Ok(r) => r,
            Err(_) => Err("panic contained at diagnostic boundary".into()),
        }
    })();
    let passed = outcome.is_ok() && cleanup["owned_excel_quit"] == true;
    println!(
        "{}",
        json!({"passed":passed,"checks":checks,"environment":outcome.as_ref().ok(),"error":outcome.as_ref().err(),"cleanup":cleanup,"limitations":["owned synthetic instance only","no event sink or busy/rejected-call proof yet","no production readiness claim"]})
    );
    if !passed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_axes_lower_bounds_and_owned_element_copies() {
        let _sta = Apartment::new().expect("STA");
        let mut array = OwnedVariant::array(2, 3, 5, 7).expect("array");
        for row in 5..=6 {
            for col in 7..=9 {
                let text = OwnedVariant::text(&format!("{row}:{col}"));
                array.array_put(row, col, &text).expect("valid coordinates");
                // The source BSTR is cleared here. The SAFEARRAY must retain its own copy.
            }
        }
        assert!(array.array_put(4, 7, &OwnedVariant::number(1.0)).is_err());
        let actual = array.array_snapshot().expect("snapshot");
        assert_eq!(actual["bounds"], json!([[5, 6], [7, 9]]));
        let cells: Vec<_> = actual["cells"].as_array().unwrap().iter().map(|v|v["value"].as_str().unwrap()).collect();
        assert_eq!(cells, ["5:7", "5:8", "5:9", "6:7", "6:8", "6:9"]);
    }

    #[test]
    fn worksheet_error_is_distinct_from_equal_numeric_value() {
        let error = OwnedVariant::error(-2146826281).scalar().unwrap();
        let number = OwnedVariant::number(-2146826281.0).scalar().unwrap();
        assert_eq!(error["kind"], "error");
        assert_eq!(number["kind"], "number");
        assert_ne!(error, number);
        assert_ne!(OwnedVariant::missing().vt(), OwnedVariant::default().vt());
    }
}
