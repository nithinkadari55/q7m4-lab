//! Disposable STA event diagnostics; production behavior requires separate validation.
//! Add `mod event_sink;` and `windows-core = "=0.62.2"` alongside windows =0.62.2.
//! windows-core is required by absolute paths emitted by interface/implement.
//! All COM handles, this subscription and its Observer remain on one COM STA.
//! No callback queries Excel or retains caller-owned VARIANT/pointer data.
#![allow(non_snake_case)]
// This reusable diagnostic module exposes entry points before the live harness
// wires every one of them; unused diagnostic APIs are intentional.
#![allow(dead_code)]

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    thread::{self, ThreadId},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            DISP_E_BADPARAMCOUNT, DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNINTERFACE,
            DISP_E_UNKNOWNNAME, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED, WAIT_FAILED,
            WAIT_TIMEOUT,
        },
        System::{
            Com::{
                DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPPARAMS, EXCEPINFO,
                IConnectionPoint, IConnectionPointContainer, IDispatch, IDispatch_Impl,
                IDispatch_Vtbl, ITypeInfo,
            },
            Variant::VARIANT,
        },
        UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
            PeekMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
        },
    },
    core::{Error, GUID, HRESULT, IUnknown, Interface, PCWSTR, Result, implement, interface},
};

pub const APP_EVENTS_IID: GUID = GUID::from_u128(0x00024413_0000_0000_c000_000000000046);
pub const SHEET_CALCULATE: i32 = 1563;
pub const SHEET_CHANGE: i32 = 1564;
pub const AFTER_CALCULATE: i32 = 2612;
const MAX_CAPACITY: usize = 4096;
const MAX_CALLBACK_ARGS: u32 = 32;

// A dispinterface adds no vtable methods: event methods arrive via IDispatch::Invoke.
#[interface("00024413-0000-0000-C000-000000000046")]
unsafe trait ExcelAppEvents: IDispatch {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallbackRecord {
    pub dispid: i32,
    pub argument_count: u32,
    pub named_argument_count: u32,
    pub named_ids: [i32; 4],
    pub named_ids_truncated: bool,
    // Rust's stable opaque thread ID, not a Windows DWORD thread identifier.
    pub thread_id: ThreadId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObserverStats {
    pub owner_thread: ThreadId,
    pub accepted: u64,
    pub invoke_attempts: u64,
    pub last_raw_dispid: i32,
    pub last_raw_flags: u16,
    pub type_info_calls: u64,
    pub type_info_count_calls: u64,
    pub last_callback_error: Option<HRESULT>,
    pub dropped: u64,
    pub unknown: u64,
    pub malformed: u64,
    pub faulted: bool,
    pub dirty: bool,
    pub closing: bool,
    pub sink_dropped: bool,
    pub last_detach_error: Option<HRESULT>,
}

struct State {
    owner_thread: ThreadId,
    capacity: usize,
    queue: RefCell<VecDeque<CallbackRecord>>,
    accepted: Cell<u64>,
    invoke_attempts: Cell<u64>,
    last_raw_dispid: Cell<i32>,
    last_raw_flags: Cell<u16>,
    type_info_calls: Cell<u64>,
    type_info_count_calls: Cell<u64>,
    last_callback_error: Cell<Option<HRESULT>>,
    dropped: Cell<u64>,
    unknown: Cell<u64>,
    malformed: Cell<u64>,
    faulted: Cell<bool>,
    dirty: Cell<bool>,
    closing: Cell<bool>,
    sink_dropped: Cell<bool>,
    last_detach_error: Cell<Option<HRESULT>>,
    #[cfg(test)]
    panic_next: Cell<bool>,
}

fn increment(counter: &Cell<u64>) {
    counter.set(counter.get().saturating_add(1));
}

/// Rc makes the observer and every subscription containing it !Send and !Sync.
#[derive(Clone)]
pub struct Observer(Rc<State>);

impl Observer {
    fn new(capacity: usize) -> Result<Self> {
        if !(1..=MAX_CAPACITY).contains(&capacity) {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        Ok(Self(Rc::new(State {
            owner_thread: thread::current().id(),
            capacity,
            queue: RefCell::new(VecDeque::with_capacity(capacity)),
            accepted: Cell::new(0),
            invoke_attempts: Cell::new(0),
            last_raw_dispid: Cell::new(0),
            last_raw_flags: Cell::new(0),
            type_info_calls: Cell::new(0),
            type_info_count_calls: Cell::new(0),
            last_callback_error: Cell::new(None),
            dropped: Cell::new(0),
            unknown: Cell::new(0),
            malformed: Cell::new(0),
            faulted: Cell::new(false),
            dirty: Cell::new(false),
            closing: Cell::new(false),
            sink_dropped: Cell::new(false),
            last_detach_error: Cell::new(None),
            #[cfg(test)]
            panic_next: Cell::new(false),
        })))
    }

    pub fn stats(&self) -> ObserverStats {
        let state = &self.0;
        ObserverStats {
            owner_thread: state.owner_thread,
            accepted: state.accepted.get(),
            invoke_attempts: state.invoke_attempts.get(),
            last_raw_dispid: state.last_raw_dispid.get(),
            last_raw_flags: state.last_raw_flags.get(),
            type_info_calls: state.type_info_calls.get(),
            type_info_count_calls: state.type_info_count_calls.get(),
            last_callback_error: state.last_callback_error.get(),
            dropped: state.dropped.get(),
            unknown: state.unknown.get(),
            malformed: state.malformed.get(),
            faulted: state.faulted.get(),
            dirty: state.dirty.get(),
            closing: state.closing.get(),
            sink_dropped: state.sink_dropped.get(),
            last_detach_error: state.last_detach_error.get(),
        }
    }

    /// Drain outside COM calls; no borrowed queue guard escapes this method.
    pub fn take_records(&self) -> Result<Vec<CallbackRecord>> {
        let mut queue = self
            .0
            .queue
            .try_borrow_mut()
            .map_err(|_| Error::from_hresult(E_UNEXPECTED))?;
        Ok(queue.drain(..).collect())
    }

    /// A known observation gap needs reconciliation even if no event was delivered.
    pub fn mark_gap(&self) {
        self.0.dirty.set(true);
    }

    /// Only after an independent current-state reconciliation by the owner.
    pub fn mark_reconciled(&self) {
        self.0.dirty.set(false);
    }
}

#[implement(ExcelAppEvents, Agile = false)]
struct ExcelSink {
    observer: Observer,
}

impl Drop for ExcelSink {
    fn drop(&mut self) {
        // No logging, COM calls, allocation, or panic during COM Release.
        self.observer.0.sink_dropped.set(true);
    }
}

impl ExcelAppEvents_Impl for ExcelSink_Impl {}

fn contained(state: &State, body: impl FnOnce() -> Result<()>) -> Result<()> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => {
            state.faulted.set(true);
            state.dirty.set(true);
            // A custom panic payload destructor could itself panic. This diagnostic
            // intentionally leaks that payload rather than unwind through COM.
            std::mem::forget(payload);
            Err(Error::from_hresult(E_UNEXPECTED))
        }
    }
}

fn known_argument_count(dispid: i32) -> Option<u32> {
    match dispid {
        SHEET_CALCULATE | 1567 => Some(1), // SheetCalculate / WorkbookOpen
        SHEET_CHANGE | 1570 | 1573 => Some(2), // Change / BeforeClose / NewSheet
        AFTER_CALCULATE => Some(0),
        _ => None,
    }
}

impl ExcelSink {
    fn record(
        &self,
        dispid: i32,
        argument_count: u32,
        named_argument_count: u32,
        named_ids: [i32; 4],
    ) -> Result<()> {
        let state = &self.observer.0;
        let thread_id = thread::current().id();
        if thread_id != state.owner_thread {
            state.faulted.set(true);
            state.dirty.set(true);
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
        // Every observation is a coarse invalidation, including unknown events.
        state.dirty.set(true);
        if state.closing.get() {
            increment(&state.dropped);
            return Ok(());
        }
        let Ok(mut queue) = state.queue.try_borrow_mut() else {
            increment(&state.dropped);
            return Ok(());
        };
        if queue.len() >= state.capacity {
            increment(&state.dropped);
            return Ok(());
        }
        // with_capacity + the hard length bound prevents callback queue growth.
        queue.push_back(CallbackRecord {
            dispid,
            argument_count,
            named_argument_count,
            named_ids,
            named_ids_truncated: named_argument_count > 4,
            thread_id,
        });
        increment(&state.accepted);
        Ok(())
    }
}

#[allow(clippy::not_unsafe_ptr_arg_deref)]
impl IDispatch_Impl for ExcelSink_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        // Generated thunk owns output handling.
        increment(&self.observer.0.type_info_count_calls);
        Ok(0)
    }

    fn GetTypeInfo(&self, _itinfo: u32, _lcid: u32) -> Result<ITypeInfo> {
        increment(&self.observer.0.type_info_calls);
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        _names: *const PCWSTR,
        _count: u32,
        _lcid: u32,
        _dispids: *mut i32,
    ) -> Result<()> {
        Err(Error::from_hresult(DISP_E_UNKNOWNNAME))
    }

    fn Invoke(
        &self,
        dispid: i32,
        riid: *const GUID,
        _lcid: u32,
        flags: DISPATCH_FLAGS,
        params: *const DISPPARAMS,
        result: *mut VARIANT,
        _exception: *mut EXCEPINFO,
        _argument_error: *mut u32,
    ) -> Result<()> {
        let state = &self.observer.0;
        // All validation and diagnostic fault injection are INSIDE this boundary.
        let outcome = contained(state, || {
            increment(&state.invoke_attempts);
            state.last_raw_dispid.set(dispid);
            state.last_raw_flags.set(flags.0);
            #[cfg(test)]
            if state.panic_next.replace(false) {
                panic!("injected event-sink diagnostic panic");
            }
            let validation = (|| -> Result<(u32, u32, [i32; 4])> {
                if riid.is_null() || params.is_null() {
                    return Err(Error::from_hresult(E_POINTER));
                }
                // ABI precondition: nonnull pointers supplied by COM are readable.
                // This check cannot protect against an arbitrary invalid address.
                if unsafe { *riid } != GUID::zeroed() {
                    return Err(Error::from_hresult(DISP_E_UNKNOWNINTERFACE));
                }
                // Excel 16.0 build 18526 was observed sending METHOD|PROPERTYGET.
                let permitted_flags =
                    flags == DISPATCH_METHOD || flags == (DISPATCH_METHOD | DISPATCH_PROPERTYGET);
                if !permitted_flags || dispid < 0 {
                    return Err(Error::from_hresult(DISP_E_MEMBERNOTFOUND));
                }
                let params = unsafe { &*params };
                if params.cArgs > MAX_CALLBACK_ARGS {
                    return Err(Error::from_hresult(DISP_E_BADPARAMCOUNT));
                }
                if params.cNamedArgs > params.cArgs {
                    return Err(Error::from_hresult(DISP_E_BADPARAMCOUNT));
                }
                if params.cNamedArgs != 0 && params.rgdispidNamedArgs.is_null() {
                    return Err(Error::from_hresult(E_POINTER));
                }
                if params.cArgs != 0 && params.rgvarg.is_null() {
                    return Err(Error::from_hresult(E_POINTER));
                }
                match known_argument_count(dispid) {
                    Some(expected) if params.cArgs != expected => {
                        return Err(Error::from_hresult(DISP_E_BADPARAMCOUNT));
                    }
                    Some(_) => {}
                    None => {
                        // Future DISPIDs are observed, not mistaken for errors.
                        increment(&state.unknown);
                    }
                }
                // Copy only bounded scalar DISPIDs. Caller storage is never retained.
                let mut named_ids = [0; 4];
                for (index, slot) in named_ids
                    .iter_mut()
                    .enumerate()
                    .take(params.cNamedArgs as usize)
                {
                    *slot = unsafe { *params.rgdispidNamedArgs.add(index) };
                }
                Ok((params.cArgs, params.cNamedArgs, named_ids))
            })();
            let (count, named_count, named_ids) = match validation {
                Ok(count) => count,
                Err(error) => {
                    increment(&state.malformed);
                    state.dirty.set(true);
                    return Err(error);
                }
            };
            if !result.is_null() {
                // COM [out] storage is not an existing owned value to clear.
                unsafe { result.write(VARIANT::default()) };
            }
            self.record(dispid, count, named_count, named_ids)
        });
        if let Err(error) = &outcome {
            state.last_callback_error.set(Some(error.code()));
        }
        outcome
    }
}

/// Holds the source's connection and a local strong sink reference. Drop only on
/// the same initialized STA; explicit detach reports failures before app shutdown.
pub struct EventSubscription {
    point: IConnectionPoint,
    cookie: Option<u32>,
    sink: Option<ExcelAppEvents>,
    observer: Observer,
}

impl EventSubscription {
    pub fn subscribe(application: &IDispatch, capacity: usize) -> Result<Self> {
        let container: IConnectionPointContainer = application.cast()?;
        let point = unsafe { container.FindConnectionPoint(&APP_EVENTS_IID) }?;
        Self::from_connection_point(point, capacity)
    }

    pub fn from_connection_point(point: IConnectionPoint, capacity: usize) -> Result<Self> {
        if unsafe { point.GetConnectionInterface() }? != APP_EVENTS_IID {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        let observer = Observer::new(capacity)?;
        let sink: ExcelAppEvents = ExcelSink {
            observer: observer.clone(),
        }
        .into();
        // Custom #[interface] has no implicit Param<IUnknown> hierarchy. Use QI.
        let unknown: IUnknown = sink.cast()?;
        let cookie = unsafe { point.Advise(&unknown) }?;
        Ok(Self {
            point,
            cookie: Some(cookie),
            sink: Some(sink),
            observer,
        })
    }

    pub fn observer(&self) -> Observer {
        self.observer.clone()
    }

    /// Idempotent after success. A failed Unadvise retains the cookie and sink so
    /// the caller can retry/report; Drop makes one final best-effort attempt.
    pub fn detach(&mut self) -> Result<()> {
        let state = &self.observer.0;
        if thread::current().id() != state.owner_thread {
            state.faulted.set(true);
            state.dirty.set(true);
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
        state.closing.set(true);
        if let Some(cookie) = self.cookie {
            if let Err(error) = unsafe { self.point.Unadvise(cookie) } {
                state.last_detach_error.set(Some(error.code()));
                state.dirty.set(true);
                return Err(error);
            }
            self.cookie = None;
        }
        self.sink = None;
        Ok(())
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        // The real COM call is fallible. Observer retains its failure diagnostic.
        let _ = self.detach();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PumpReport {
    pub dispatched_messages: usize,
    pub quit_seen: bool,
    pub message_limit_reached: bool,
}

/// Scheduling bound, not preemption: a dispatched/native call may itself block.
/// A separate watchdog is required for a hard process-level deadline.
pub fn pump_for(budget: Duration, max_messages: usize) -> Result<PumpReport> {
    if budget > Duration::from_secs(30) || !(1..=100_000).contains(&max_messages) {
        return Err(Error::from_hresult(E_INVALIDARG));
    }
    let start = Instant::now();
    let mut report = PumpReport::default();
    while start.elapsed() < budget {
        let mut message = MSG::default();
        while start.elapsed() < budget
            && report.dispatched_messages < max_messages
            && unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool()
        {
            if message.message == WM_QUIT {
                report.quit_seen = true;
                return Ok(report);
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            report.dispatched_messages += 1;
        }
        if report.dispatched_messages >= max_messages {
            report.message_limit_reached = true;
            break;
        }
        let remaining = budget.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        // At most 10ms of idle wait; never a plain sleep/channel blocking receive.
        let millis = remaining.as_millis().clamp(1, 10) as u32;
        let status =
            unsafe { MsgWaitForMultipleObjectsEx(None, millis, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        if status == WAIT_FAILED {
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
        // No handles: the only expected outcomes are messages (0) or timeout.
        if status != WAIT_TIMEOUT && status.0 != 0 {
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::{
        Win32::System::Com::{IConnectionPoint_Impl, IEnumConnections},
        core::Ref,
    };

    fn new_sink(capacity: usize) -> (ExcelAppEvents, IDispatch, Observer) {
        let observer = Observer::new(capacity).expect("bounded capacity");
        let sink: ExcelAppEvents = ExcelSink {
            observer: observer.clone(),
        }
        .into();
        let dispatch = sink.cast::<IDispatch>().expect("inherited IDispatch QI");
        (sink, dispatch, observer)
    }

    fn invoke(dispatch: &IDispatch, dispid: i32, count: usize) -> Result<()> {
        // VARIANT itself is not Clone in windows 0.62.2. Construct each empty
        // value independently; never introduce a shallow owning-payload clone.
        let mut args: Vec<VARIANT> = (0..count).map(|_| VARIANT::default()).collect();
        let params = DISPPARAMS {
            rgvarg: if args.is_empty() {
                std::ptr::null_mut()
            } else {
                args.as_mut_ptr()
            },
            cArgs: count as u32,
            ..Default::default()
        };
        unsafe {
            dispatch.Invoke(
                dispid,
                &GUID::zeroed(),
                0,
                DISPATCH_METHOD,
                &params,
                None,
                None,
                None,
            )
        }
    }

    fn assert_code(result: Result<()>, code: HRESULT) {
        assert_eq!(result.expect_err("expected HRESULT failure").code(), code);
    }

    #[test]
    fn qi_custom_inherited_identity_unknown_and_non_agile() {
        let (sink, dispatch, observer) = new_sink(4);
        assert_eq!(ExcelAppEvents::IID, APP_EVENTS_IID);
        let custom = dispatch
            .cast::<ExcelAppEvents>()
            .expect("custom outgoing IID");
        let a: IUnknown = sink.cast().expect("IUnknown from custom");
        let b: IUnknown = dispatch.cast().expect("IUnknown from dispatch");
        assert_eq!(Interface::as_raw(&a), Interface::as_raw(&b));
        for iid in [
            GUID::from_u128(0x12de4cf7_2bd3_4d33_8c58_8ec4362ca116),
            // IAgileObject and IMarshal must not be advertised by this STA sink.
            GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90),
            GUID::from_u128(0x00000003_0000_0000_c000_000000000046),
        ] {
            let mut output = std::ptr::null_mut();
            let result = unsafe { a.query(&iid, &mut output) };
            assert_eq!(result, HRESULT(0x80004002_u32 as i32));
            assert!(output.is_null());
        }
        drop((a, b, custom, dispatch));
        assert!(!observer.stats().sink_dropped);
        drop(sink);
        assert!(observer.stats().sink_dropped);
    }

    #[test]
    fn excel_combined_method_get_flag_is_accepted_and_property_only_is_rejected() {
        let (_sink, dispatch, observer) = new_sink(4);
        let params = DISPPARAMS::default();
        for flags in [DISPATCH_METHOD | DISPATCH_PROPERTYGET, DISPATCH_PROPERTYGET] {
            let outcome = unsafe {
                dispatch.Invoke(
                    AFTER_CALCULATE,
                    &GUID::zeroed(),
                    0,
                    flags,
                    &params,
                    None,
                    None,
                    None,
                )
            };
            if flags == DISPATCH_PROPERTYGET {
                assert_code(outcome, DISP_E_MEMBERNOTFOUND);
            } else {
                outcome.expect("observed Excel flags accepted");
            }
        }
        assert_eq!(observer.take_records().unwrap().len(), 1);
        assert_eq!(observer.stats().malformed, 1);
    }

    #[test]
    fn named_event_argument_ids_are_copied_and_missing_storage_is_rejected() {
        let (_sink, dispatch, observer) = new_sink(4);
        let mut args = [VARIANT::default(), VARIANT::default()];
        let mut ids = [1, 0];
        let params = DISPPARAMS {
            rgvarg: args.as_mut_ptr(),
            rgdispidNamedArgs: ids.as_mut_ptr(),
            cArgs: 2,
            cNamedArgs: 2,
        };
        unsafe {
            dispatch.Invoke(
                SHEET_CHANGE,
                &GUID::zeroed(),
                0,
                DISPATCH_METHOD | DISPATCH_PROPERTYGET,
                &params,
                None,
                None,
                None,
            )
        }
        .expect("named callback");
        ids.fill(999);
        let records = observer.take_records().unwrap();
        assert_eq!(records[0].named_argument_count, 2);
        assert_eq!(records[0].named_ids, [1, 0, 0, 0]);
        assert!(!records[0].named_ids_truncated);
        let missing = DISPPARAMS {
            rgdispidNamedArgs: std::ptr::null_mut(),
            ..params
        };
        assert_code(
            unsafe {
                dispatch.Invoke(
                    SHEET_CHANGE,
                    &GUID::zeroed(),
                    0,
                    DISPATCH_METHOD,
                    &missing,
                    None,
                    None,
                    None,
                )
            },
            E_POINTER,
        );
    }

    #[test]
    fn callbacks_are_owned_bounded_records_and_unknown_is_observed() {
        let (_sink, dispatch, observer) = new_sink(4);
        invoke(&dispatch, SHEET_CHANGE, 2).expect("known event");
        invoke(&dispatch, AFTER_CALCULATE, 0).expect("zero arguments may be null");
        invoke(&dispatch, 900_001, 3).expect("future positive event is acknowledged");
        let records = observer.take_records().expect("owned drain");
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].dispid, SHEET_CHANGE);
        assert_eq!(records[0].argument_count, 2);
        assert!(
            records
                .iter()
                .all(|r| r.thread_id == thread::current().id())
        );
        assert_eq!(observer.stats().unknown, 1);
        assert!(observer.stats().dirty);
        observer.mark_reconciled();
        assert!(!observer.stats().dirty);
    }

    #[test]
    fn malformed_callbacks_fail_inside_boundary_without_enqueuing() {
        let (_sink, dispatch, observer) = new_sink(4);
        assert_code(invoke(&dispatch, SHEET_CHANGE, 1), DISP_E_BADPARAMCOUNT);
        assert_code(invoke(&dispatch, -1, 0), DISP_E_MEMBERNOTFOUND);
        assert_code(invoke(&dispatch, 900_001, 33), DISP_E_BADPARAMCOUNT);
        let null_args = DISPPARAMS {
            cArgs: 2,
            ..Default::default()
        };
        assert_code(
            unsafe {
                dispatch.Invoke(
                    SHEET_CHANGE,
                    &GUID::zeroed(),
                    0,
                    DISPATCH_METHOD,
                    &null_args,
                    None,
                    None,
                    None,
                )
            },
            E_POINTER,
        );
        assert_code(
            unsafe {
                dispatch.Invoke(
                    AFTER_CALCULATE,
                    &GUID::zeroed(),
                    0,
                    DISPATCH_METHOD,
                    std::ptr::null(),
                    None,
                    None,
                    None,
                )
            },
            E_POINTER,
        );
        let empty = DISPPARAMS::default();
        assert_code(
            unsafe {
                dispatch.Invoke(
                    AFTER_CALCULATE,
                    &APP_EVENTS_IID,
                    0,
                    DISPATCH_METHOD,
                    &empty,
                    None,
                    None,
                    None,
                )
            },
            DISP_E_UNKNOWNINTERFACE,
        );
        let named = DISPPARAMS {
            cNamedArgs: 1,
            ..Default::default()
        };
        assert_code(
            unsafe {
                dispatch.Invoke(
                    AFTER_CALCULATE,
                    &GUID::zeroed(),
                    0,
                    DISPATCH_METHOD,
                    &named,
                    None,
                    None,
                    None,
                )
            },
            DISP_E_BADPARAMCOUNT,
        );
        assert!(observer.take_records().expect("drain").is_empty());
        assert_eq!(observer.stats().malformed, 7);
        assert!(observer.stats().dirty);
    }

    #[test]
    fn full_queue_and_reentrant_borrow_set_dirty_without_blocking() {
        let (_sink, dispatch, observer) = new_sink(1);
        invoke(&dispatch, AFTER_CALCULATE, 0).expect("first event");
        observer.mark_reconciled();
        invoke(&dispatch, AFTER_CALCULATE, 0).expect("overflow is acknowledged");
        assert_eq!(observer.stats().dropped, 1);
        assert!(observer.stats().dirty);
        observer.take_records().expect("drain");
        let guard = observer.0.queue.borrow_mut();
        invoke(&dispatch, AFTER_CALCULATE, 0).expect("borrow collision is acknowledged");
        drop(guard);
        assert_eq!(observer.stats().dropped, 2);
        assert!(!observer.stats().faulted);
    }

    #[test]
    fn closing_callback_is_acknowledged_but_counts_unobserved_invalidation() {
        let (_sink, dispatch, observer) = new_sink(1);
        observer.0.closing.set(true);
        invoke(&dispatch, AFTER_CALCULATE, 0).expect("passive shutdown callback");
        assert!(observer.take_records().expect("drain").is_empty());
        assert_eq!(observer.stats().accepted, 0);
        assert_eq!(observer.stats().dropped, 1);
        assert!(observer.stats().dirty);
    }

    #[test]
    fn injected_panic_is_caught_even_before_invalid_argument_checks() {
        let (_sink, dispatch, observer) = new_sink(1);
        observer.0.panic_next.set(true);
        assert_code(
            unsafe {
                dispatch.Invoke(
                    AFTER_CALCULATE,
                    std::ptr::null(),
                    0,
                    DISPATCH_METHOD,
                    std::ptr::null(),
                    None,
                    None,
                    None,
                )
            },
            E_UNEXPECTED,
        );
        assert!(observer.stats().faulted);
        assert!(observer.stats().dirty);
        assert_eq!(observer.stats().malformed, 0);
        invoke(&dispatch, AFTER_CALCULATE, 0).expect("object remains callable");
    }

    #[derive(Default)]
    struct MockState {
        sink: RefCell<Option<ExcelAppEvents>>,
        unadvises: Cell<u32>,
        fail_unadvise: Cell<bool>,
    }

    #[implement(IConnectionPoint, Agile = false)]
    struct MockPoint {
        state: Rc<MockState>,
    }

    impl IConnectionPoint_Impl for MockPoint_Impl {
        fn GetConnectionInterface(&self) -> Result<GUID> {
            Ok(APP_EVENTS_IID)
        }
        fn GetConnectionPointContainer(&self) -> Result<IConnectionPointContainer> {
            Err(Error::from_hresult(E_NOTIMPL))
        }
        fn Advise(&self, sink: Ref<IUnknown>) -> Result<u32> {
            if self.state.sink.borrow().is_some() {
                return Err(Error::from_hresult(E_UNEXPECTED));
            }
            // Mimics a real source's required outgoing-IID QI and strong retain.
            let custom: ExcelAppEvents = sink.ok()?.cast()?;
            *self.state.sink.borrow_mut() = Some(custom);
            Ok(41)
        }
        fn Unadvise(&self, cookie: u32) -> Result<()> {
            self.state.unadvises.set(self.state.unadvises.get() + 1);
            if self.state.fail_unadvise.get() {
                return Err(Error::from_hresult(E_UNEXPECTED));
            }
            if cookie != 41 || self.state.sink.borrow().is_none() {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            *self.state.sink.borrow_mut() = None;
            Ok(())
        }
        fn EnumConnections(&self) -> Result<IEnumConnections> {
            Err(Error::from_hresult(E_NOTIMPL))
        }
    }

    fn subscription() -> (EventSubscription, Rc<MockState>) {
        let state = Rc::new(MockState::default());
        let point: IConnectionPoint = MockPoint {
            state: state.clone(),
        }
        .into();
        (
            EventSubscription::from_connection_point(point, 4).expect("mock Advise"),
            state,
        )
    }

    #[test]
    fn explicit_detach_is_idempotent_and_releases_source_and_local_refs() {
        let (mut subscription, mock) = subscription();
        let observer = subscription.observer();
        assert!(!observer.stats().sink_dropped);
        subscription.detach().expect("Unadvise");
        assert!(mock.sink.borrow().is_none());
        assert!(observer.stats().sink_dropped);
        subscription.detach().expect("idempotent");
        drop(subscription);
        assert_eq!(mock.unadvises.get(), 1);
    }

    #[test]
    fn drop_unadvises_before_releasing_final_sink() {
        let (subscription, mock) = subscription();
        let observer = subscription.observer();
        drop(subscription);
        assert_eq!(mock.unadvises.get(), 1);
        assert!(mock.sink.borrow().is_none());
        assert!(observer.stats().closing);
        assert!(observer.stats().sink_dropped);
    }

    #[test]
    fn failed_detach_retains_cookie_sink_and_error_for_retry() {
        let (mut subscription, mock) = subscription();
        let observer = subscription.observer();
        mock.fail_unadvise.set(true);
        assert_code(subscription.detach(), E_UNEXPECTED);
        assert!(mock.sink.borrow().is_some());
        assert!(!observer.stats().sink_dropped);
        assert_eq!(observer.stats().last_detach_error, Some(E_UNEXPECTED));
        mock.fail_unadvise.set(false);
        subscription.detach().expect("retry same retained cookie");
        assert!(observer.stats().sink_dropped);
        drop(subscription);
        assert_eq!(mock.unadvises.get(), 2);
    }

    #[test]
    fn pump_rejects_unbounded_inputs_and_zero_budget_never_dispatches() {
        assert_eq!(
            pump_for(Duration::ZERO, 1).expect("zero budget"),
            PumpReport::default()
        );
        assert_eq!(
            pump_for(Duration::from_secs(31), 1)
                .expect_err("bound")
                .code(),
            E_INVALIDARG
        );
        assert_eq!(
            pump_for(Duration::ZERO, 0)
                .expect_err("message bound")
                .code(),
            E_INVALIDARG
        );
        assert!(Observer::new(0).is_err());
        assert!(Observer::new(MAX_CAPACITY + 1).is_err());
    }
}
