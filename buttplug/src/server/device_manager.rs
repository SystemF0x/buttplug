// Buttplug Rust Source Code File - See https://buttplug.io for more info.
//
// Copyright 2016-2019 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

//! Buttplug Device Manager, manages Device Subtype (Platform/Communication bus
//! specific) Managers

use super::{
  comm_managers::{
    DeviceCommunicationEvent, DeviceCommunicationManager, DeviceCommunicationManagerBuilder,
  },
  device_manager_event_loop::DeviceManagerEventLoop,
  ping_timer::PingTimer,
  ButtplugServerError,
};
use crate::{
  core::{
    errors::{ButtplugDeviceError, ButtplugMessageError, ButtplugUnknownError},
    messages::{
      self, ButtplugClientMessage, ButtplugDeviceCommandMessageUnion,
      ButtplugDeviceManagerMessageUnion, ButtplugDeviceMessage, ButtplugMessage,
      ButtplugServerMessage, DeviceList, DeviceMessageInfo,
    },
  },
  device::{
    configuration_manager::{DeviceConfigurationManager, ProtocolDefinition}, protocol::ButtplugProtocol, ButtplugDevice,
  },
  server::ButtplugServerResultFuture,
  util::async_manager,
};
use dashmap::{DashMap, DashSet};
use futures::future;
use std::{
  convert::TryFrom,
  sync::{atomic::Ordering, Arc},
};
use tokio::sync::{broadcast, mpsc};

pub struct DeviceManager {
  // This uses a map to make sure we don't have 2 comm managers of the same type
  // register. Also means we can do lockless access since it's a Dashmap.
  comm_managers: Arc<DashMap<String, Box<dyn DeviceCommunicationManager>>>,
  devices: Arc<DashMap<u32, Arc<ButtplugDevice>>>,
  device_allow_list: Arc<DashSet<String>>,
  device_deny_list: Arc<DashSet<String>>,
  device_event_sender: mpsc::Sender<DeviceCommunicationEvent>,
  config: Arc<DeviceConfigurationManager>,
}

unsafe impl Send for DeviceManager {}

unsafe impl Sync for DeviceManager {}

impl DeviceManager {
  pub fn new(
    output_sender: broadcast::Sender<ButtplugServerMessage>,
    ping_timer: Arc<PingTimer>,
    allow_raw_messages: bool,
  ) -> Self {
    let config = Arc::new(DeviceConfigurationManager::new(allow_raw_messages));
    let devices = Arc::new(DashMap::new());
    let (device_event_sender, device_event_receiver) = mpsc::channel(256);
    let device_allow_list = Arc::new(DashSet::new());
    let device_deny_list = Arc::new(DashSet::new());
    let mut event_loop = DeviceManagerEventLoop::new(
      config.clone(),
      output_sender,
      devices.clone(),
      device_allow_list.clone(),
      device_deny_list.clone(),
      ping_timer,
      device_event_receiver,
    );
    async_manager::spawn(async move {
      event_loop.run().await;
    })
    .unwrap();
    Self {
      device_event_sender,
      devices,
      device_allow_list,
      device_deny_list,
      comm_managers: Arc::new(DashMap::new()),
      config,
    }
  }

  fn start_scanning(&self) -> ButtplugServerResultFuture {
    if self.comm_managers.is_empty() {
      ButtplugUnknownError::NoDeviceCommManagers.into()
    } else {
      let mgrs = self.comm_managers.clone();
      let sender = self.device_event_sender.clone();
      Box::pin(async move {
        for mgr in mgrs.iter() {
          if mgr.value().scanning_status().load(Ordering::SeqCst) {
            return Err(ButtplugDeviceError::DeviceScanningAlreadyStarted.into());
          }
        }
        let fut_vec: Vec<_> = mgrs
          .iter()
          .map(|guard| guard.value().start_scanning())
          .collect();
        // TODO If start_scanning fails anywhere, this will ignore it. We should maybe at least log?
        future::join_all(fut_vec).await;
        debug!("All managers started, sending ScanningStarted (and invoking ScanningFinished hack) signal to event loop.");
        // HACK: In case everything somehow exited between the time all of our
        // futures resolved and when we updated the event loop, act like we're a
        // device comm manager and send a ScanningFinished message. This will
        // cause the finish check to run just in case, so we don't get stuck.
        //
        // Ideally, this should be some sort of state machine, but for now, we
        // can deal with this.
        //
        // At this point, it doesn't really matter what we return, only way that
        // event loop could shut down is if the whole system is shutting down.
        // So complain if our sends error out, but don't worry about returning
        // an error.
        if sender
          .send(DeviceCommunicationEvent::ScanningStarted)
          .await
          .is_err()
          || sender
            .send(DeviceCommunicationEvent::ScanningFinished)
            .await
            .is_err()
        {
          debug!("Device manager event loop shut down, cannot send ScanningStarted");
        }
        Ok(messages::Ok::default().into())
      })
    }
  }

  fn stop_scanning(&self) -> ButtplugServerResultFuture {
    if self.comm_managers.is_empty() {
      ButtplugUnknownError::NoDeviceCommManagers.into()
    } else {
      let mgrs = self.comm_managers.clone();
      Box::pin(async move {
        let mut scanning_stopped = true;
        for mgr in mgrs.iter() {
          if mgr.value().scanning_status().load(Ordering::SeqCst) {
            debug!("Device manager {} has not stopped scanning yet.", mgr.key());
            scanning_stopped = false;
            break;
          }
        }
        if scanning_stopped {
          return Err(ButtplugDeviceError::DeviceScanningAlreadyStopped.into());
        }

        let fut_vec: Vec<_> = mgrs
          .iter()
          .map(|guard| guard.value().stop_scanning())
          .collect();
        // TODO If stop_scanning fails anywhere, this will ignore it. We should maybe at least log?
        future::join_all(fut_vec).await;
        Ok(messages::Ok::default().into())
      })
    }
  }

  fn stop_all_devices(&self) -> ButtplugServerResultFuture {
    let device_map = self.devices.clone();
    // TODO This could use some error reporting.
    Box::pin(async move {
      let fut_vec: Vec<_> = device_map
        .iter()
        .map(|dev| {
          let device = dev.value();
          device.parse_message(messages::StopDeviceCmd::new(1).into())
        })
        .collect();
      future::join_all(fut_vec).await;
      Ok(messages::Ok::default().into())
    })
  }

  fn parse_device_message(
    &self,
    device_msg: ButtplugDeviceCommandMessageUnion,
  ) -> ButtplugServerResultFuture {
    match self.devices.get(&device_msg.device_index()) {
      Some(device) => {
        let fut = device.parse_message(device_msg);
        // Create a future to run the message through the device, then handle adding the id to the result.
        Box::pin(async move { fut.await })
      }
      None => ButtplugDeviceError::DeviceNotAvailable(device_msg.device_index()).into(),
    }
  }

  fn parse_device_manager_message(
    &self,
    manager_msg: ButtplugDeviceManagerMessageUnion,
  ) -> ButtplugServerResultFuture {
    match manager_msg {
      ButtplugDeviceManagerMessageUnion::RequestDeviceList(msg) => {
        let devices = self
          .devices
          .iter()
          .map(|device| {
            let dev = device.value();
            DeviceMessageInfo::new(*device.key(), &dev.name(), dev.message_attributes())
          })
          .collect();
        let mut device_list = DeviceList::new(devices);
        device_list.set_id(msg.id());
        Box::pin(future::ready(Ok(device_list.into())))
      }
      ButtplugDeviceManagerMessageUnion::StopAllDevices(_) => self.stop_all_devices(),
      ButtplugDeviceManagerMessageUnion::StartScanning(_) => self.start_scanning(),
      ButtplugDeviceManagerMessageUnion::StopScanning(_) => self.stop_scanning(),
    }
  }

  pub fn parse_message(&self, msg: ButtplugClientMessage) -> ButtplugServerResultFuture {
    // If this is a device command message, just route it directly to the
    // device.
    match ButtplugDeviceCommandMessageUnion::try_from(msg.clone()) {
      Ok(device_msg) => self.parse_device_message(device_msg),
      Err(_) => match ButtplugDeviceManagerMessageUnion::try_from(msg.clone()) {
        Ok(manager_msg) => self.parse_device_manager_message(manager_msg),
        Err(_) => ButtplugMessageError::UnexpectedMessageType(format!("{:?}", msg)).into(),
      },
    }
  }

  pub fn add_comm_manager<T>(&self, builder: T) -> Result<(), ButtplugServerError>
  where
    T: DeviceCommunicationManagerBuilder,
  {
    let mgr = builder
      .event_sender(self.device_event_sender.clone())
      .finish();
    if self.comm_managers.contains_key(mgr.name()) {
      return Err(ButtplugServerError::DeviceManagerTypeAlreadyAdded(
        mgr.name().to_owned(),
      ));
    }
    let status = mgr.scanning_status();
    let sender = self.device_event_sender.clone();
    // TODO This could run out of order and possibly cause weird scanning finished bugs?
    async_manager::spawn(async move {
      sender
        .send(DeviceCommunicationEvent::DeviceManagerAdded(status))
        .await
        .unwrap();
    })
    .unwrap();
    self.comm_managers.insert(mgr.name().to_owned(), mgr);
    Ok(())
  }

  pub fn add_protocol<T>(&self, protocol_name: &str) -> Result<(), ButtplugServerError>
  where
    T: ButtplugProtocol,
  {
    if !self.config.has_protocol(protocol_name) {
      self.config.add_protocol::<T>(protocol_name);
      Ok(())
    } else {
      Err(ButtplugServerError::ProtocolAlreadyAdded(
        protocol_name.to_owned(),
      ))
    }
  }

  pub fn remove_protocol(&self, protocol_name: &str) -> Result<(), ButtplugServerError> {
    if self.config.has_protocol(protocol_name) {
      self.config.remove_protocol(protocol_name);
      Ok(())
    } else {
      Err(ButtplugServerError::ProtocolDoesNotExist(
        protocol_name.to_owned(),
      ))
    }
  }

  pub fn remove_all_protocols(&self) {
    self.config.remove_all_protocols();
  }

  pub fn add_protocol_definition(&self, name: &str, config: ProtocolDefinition) {
    self.config.add_protocol_definition(name, config);
  }

  pub fn remove_protocol_definition(&self, name: &str) {
    self.config.remove_protocol_definition(name);    
  }

  pub fn add_allowed_device(&self, address: &str) {
    info!("Adding device address {} to allowed devices list.", address);
    self.device_allow_list.insert(address.to_owned());
  }

  pub fn add_denied_device(&self, address: &str) {
    info!("Adding device address {} to denied devices list.", address);
    self.device_deny_list.insert(address.to_owned());
  }

  pub fn remove_allowed_device(&self, address: &str) {
    info!("Removing device address {} from allowed devices list.", address);
    self.device_allow_list.remove(address);
  }

  pub fn remove_denied_device(&self, address: &str) {
    info!("Removing device address {} from denied devices list.", address);
    self.device_deny_list.remove(address);
  }
}

impl Drop for DeviceManager {
  fn drop(&mut self) {
    info!("Dropping device manager!");
  }
}
