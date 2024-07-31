use crate::utilities::mutex::Mutex;
use crate::{ble, enums::*, utilities::voidp_to_ref, BLEAdvertisedDevice, BLEError, Signal};
use alloc::{boxed::Box, vec::Vec};
use core::ffi::c_void;
use esp_idf_svc::sys as esp_idf_sys;

pub struct BLEScan {
  #[allow(clippy::type_complexity)]
  on_result: Option<Box<dyn FnMut(&mut Self, &BLEAdvertisedDevice) + Send + Sync>>,
  on_completed: Option<Box<dyn FnMut() + Send + Sync>>,
  scan_params: esp_idf_sys::ble_gap_disc_params,
  stopped: bool,
  scan_results: Vec<BLEAdvertisedDevice>,
  signal: Signal<()>,
}

type CbArgType<'a> = (
  &'a mut BLEScan,
  Option<&'a mut (dyn FnMut(&mut BLEScan, &BLEAdvertisedDevice) + Send + Sync)>,
);

impl BLEScan {
  pub(crate) fn new() -> Self {
    let mut ret = Self {
      on_result: None,
      on_completed: None,
      scan_params: esp_idf_sys::ble_gap_disc_params {
        itvl: 0,
        window: 0,
        filter_policy: esp_idf_sys::BLE_HCI_SCAN_FILT_NO_WL as _,
        _bitfield_align_1: [0; 0],
        _bitfield_1: esp_idf_sys::__BindgenBitfieldUnit::new([0; 1]),
      },
      stopped: true,
      scan_results: Vec::new(),
      signal: Signal::new(),
    };
    ret.limited(false);
    ret.filter_duplicates(true);
    ret.active_scan(false).interval(100).window(100);
    ret
  }

  pub fn active_scan(&mut self, active: bool) -> &mut Self {
    self.scan_params.set_passive((!active) as _);
    self
  }

  pub fn filter_duplicates(&mut self, val: bool) -> &mut Self {
    self.scan_params.set_filter_duplicates(val as _);
    self
  }

  /// Set whether or not the BLE controller only report scan results
  /// from devices advertising in limited discovery mode, i.e. directed advertising.
  pub fn limited(&mut self, val: bool) -> &mut Self {
    self.scan_params.set_limited(val as _);
    self
  }

  /// Sets the scan filter policy.
  pub fn filter_policy(&mut self, val: ScanFilterPolicy) -> &mut Self {
    self.scan_params.filter_policy = val.into();
    self
  }

  /// Set the interval to scan.
  pub fn interval(&mut self, interval_msecs: u16) -> &mut Self {
    self.scan_params.itvl = ((interval_msecs as f32) / 0.625) as u16;
    self
  }

  /// Set the window to actively scan.
  pub fn window(&mut self, window_msecs: u16) -> &mut Self {
    self.scan_params.window = ((window_msecs as f32) / 0.625) as u16;
    self
  }

  /// Set a callback to be called when a new scan result is detected.
  /// * callback first parameter: The reference to `Self`
  /// * callback second parameter: Newly found device
  pub fn on_result(
    &mut self,
    callback: impl FnMut(&mut Self, &BLEAdvertisedDevice) + Send + Sync + 'static,
  ) -> &mut Self {
    self.on_result = Some(Box::new(callback));
    self
  }

  pub fn on_completed(&mut self, callback: impl FnMut() + Send + Sync + 'static) -> &mut Self {
    self.on_completed = Some(Box::new(callback));
    self
  }

  /// Asynchronously finds a device.
  ///
  /// # Examples
  ///
  /// ```
  /// let ble_device = BLEDevice::take().unwrap();
  /// let ble_scan = ble_device.get_scan();
  /// let name = "Device Name To Be Found";
  /// let device = ble_scan.find_device(10000, |device| device.name() == name).await.unwrap();
  /// ```
  pub async fn find_device(
    &mut self,
    duration_ms: i32,
    callback: impl Fn(&BLEAdvertisedDevice) -> bool + Send + Sync,
  ) -> Result<Option<BLEAdvertisedDevice>, BLEError> {
    let result = Mutex::new(Result::Ok(None));

    let mut on_result = |scan: &mut Self, device: &BLEAdvertisedDevice| {
      if callback(device) {
        *result.lock() = scan.stop().and(Ok(Some(device.clone())));
      }
    };

    self.start_core(duration_ms, Some(&mut on_result)).await?;

    result.into_innter()
  }

  pub async fn start(&mut self, duration_ms: i32) -> Result<(), BLEError> {
    unsafe {
      let scan = self as *mut Self;

      let callback = (*scan).on_result.as_deref_mut(); // .as_ref().map(|x| x.deref_mut());
      (*scan).start_core(duration_ms, callback).await
    }
  }

  #[allow(clippy::type_complexity)]
  async fn start_core(
    &mut self,
    duration_ms: i32,
    callback: Option<&mut (dyn FnMut(&mut Self, &BLEAdvertisedDevice) + Send + Sync)>,
  ) -> Result<(), BLEError> {
    let cb_arg = (self, callback);
    unsafe {
      ble!(esp_idf_sys::ble_gap_disc(
        crate::ble_device::OWN_ADDR_TYPE as _,
        duration_ms,
        &cb_arg.0.scan_params,
        Some(Self::handle_gap_event),
        core::ptr::addr_of!(cb_arg) as _,
      ))?;
    }
    cb_arg.0.stopped = false;

    cb_arg.0.signal.wait().await;
    Ok(())
  }

  pub fn stop(&mut self) -> Result<(), BLEError> {
    self.stopped = true;
    let rc = unsafe { esp_idf_sys::ble_gap_disc_cancel() };
    if rc != 0 && rc != (esp_idf_sys::BLE_HS_EALREADY as _) {
      return BLEError::convert(rc as _);
    }

    if let Some(callback) = self.on_completed.as_mut() {
      callback();
    }
    self.signal.signal(());

    Ok(())
  }

  pub fn get_results(&mut self) -> core::slice::Iter<'_, BLEAdvertisedDevice> {
    self.scan_results.iter()
  }

  pub fn clear_results(&mut self) {
    self.scan_results.clear();
  }

  pub(crate) fn reset(&mut self) {
    self.on_result = None;
    self.on_completed = None;
  }

  extern "C" fn handle_gap_event(event: *mut esp_idf_sys::ble_gap_event, arg: *mut c_void) -> i32 {
    let event = unsafe { &*event };
    let (scan, on_result) = unsafe { voidp_to_ref::<CbArgType>(arg) };

    match event.type_ as u32 {
      esp_idf_sys::BLE_GAP_EVENT_EXT_DISC | esp_idf_sys::BLE_GAP_EVENT_DISC => {
        let disc = unsafe { &event.__bindgen_anon_1.disc };

        let mut advertised_device = scan
          .scan_results
          .iter_mut()
          .find(|x| x.addr().value.val == disc.addr.val);

        if advertised_device.is_none() {
          if disc.event_type != esp_idf_sys::BLE_HCI_ADV_RPT_EVTYPE_SCAN_RSP as _ {
            let device = BLEAdvertisedDevice::new(disc);
            scan.scan_results.push(device);
            advertised_device = scan.scan_results.last_mut();
          } else {
            return 0;
          }
        }

        let advertised_device = advertised_device.unwrap();

        let data = unsafe { core::slice::from_raw_parts(disc.data, disc.length_data as _) };
        ::log::debug!("DATA: {:X?}", data);
        advertised_device.parse_advertisement(data);

        advertised_device.update_rssi(disc.rssi);

        if let Some(callback) = on_result {
          if scan.scan_params.passive() != 0
            || (advertised_device.adv_type() != AdvType::Ind
              && advertised_device.adv_type() != AdvType::ScanInd)
            || disc.event_type == esp_idf_sys::BLE_HCI_ADV_RPT_EVTYPE_SCAN_RSP as _
          {
            let (scan, _) = unsafe { voidp_to_ref::<CbArgType>(arg) };
            callback(scan, advertised_device);
          }
        }
      }
      esp_idf_sys::BLE_GAP_EVENT_DISC_COMPLETE => {
        if let Some(callback) = scan.on_completed.as_mut() {
          callback();
        }
        scan.signal.signal(());
      }
      _ => {}
    }
    0
  }
}
