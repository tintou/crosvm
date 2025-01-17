// Copyright 2021 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use std::sync::Arc;
use sync::Mutex;

use crate::bus::{HostHotPlugKey, HotPlugBus};
use crate::pci::msix::{MsixCap, MsixConfig};
use crate::pci::pci_configuration::{
    PciBarConfiguration, PciBarPrefetchable, PciBarRegionType, PciBridgeSubclass, PciCapability,
    PciCapabilityID, PciClassCode, PciConfiguration, PciHeaderType,
};
use crate::pci::pci_device::{Error as PciDeviceError, PciDevice};
use crate::pci::pci_root::{PciAddress, PCI_VENDOR_ID_INTEL};
use crate::PciInterruptPin;
use base::{warn, AsRawDescriptor, Event, RawDescriptor, Tube};
use data_model::DataInit;
use hypervisor::Datamatch;
use resources::{Alloc, MmioType, SystemAllocator};

pub trait PcieDevice: Send {
    fn get_device_id(&self) -> u16;
    fn debug_label(&self) -> String;
    fn read_config(&self, reg_idx: usize, data: &mut u32);
    fn write_config(&mut self, reg_idx: usize, offset: u64, data: &[u8]);
    fn clone_interrupt(&mut self, msix_config: Arc<Mutex<MsixConfig>>);
    fn get_caps(&self) -> Vec<Box<dyn PciCapability>>;
    fn set_capability_reg_idx(&mut self, id: PciCapabilityID, reg_idx: usize);
}

const BR_MSIX_TABLE_OFFSET: u64 = 0x0;
const BR_MSIX_PBA_OFFSET: u64 = 0x100;
const PCI_BRIDGE_BAR_SIZE: u64 = 0x1000;
pub struct PciBridge {
    device: Box<dyn PcieDevice>,
    config: PciConfiguration,
    pci_address: Option<PciAddress>,
    setting_bar: u8,
    msix_config: Arc<Mutex<MsixConfig>>,
    msix_cap_reg_idx: Option<usize>,
    interrupt_evt: Option<Event>,
    interrupt_resample_evt: Option<Event>,
}

impl PciBridge {
    pub fn new(device: Box<dyn PcieDevice>, msi_device_tube: Tube) -> Self {
        let msix_config = Arc::new(Mutex::new(MsixConfig::new(1, msi_device_tube)));
        let device_id = device.get_device_id();
        let config = PciConfiguration::new(
            PCI_VENDOR_ID_INTEL,
            device_id,
            PciClassCode::BridgeDevice,
            &PciBridgeSubclass::PciToPciBridge,
            None,
            PciHeaderType::Bridge,
            false,
            0,
            0,
            0,
        );

        PciBridge {
            device,
            config,
            pci_address: None,
            setting_bar: 0,
            msix_config,
            msix_cap_reg_idx: None,
            interrupt_evt: None,
            interrupt_resample_evt: None,
        }
    }
}

impl PciDevice for PciBridge {
    fn debug_label(&self) -> String {
        self.device.debug_label()
    }

    fn allocate_address(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> std::result::Result<PciAddress, PciDeviceError> {
        if self.pci_address.is_none() {
            self.pci_address = match resources.allocate_pci(0, self.debug_label()) {
                Some(Alloc::PciBar {
                    bus,
                    dev,
                    func,
                    bar: _,
                }) => Some(PciAddress { bus, dev, func }),
                _ => None,
            }
        }
        self.pci_address.ok_or(PciDeviceError::PciAllocationFailed)
    }

    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut rds = Vec::new();
        if let Some(interrupt_evt) = &self.interrupt_evt {
            rds.push(interrupt_evt.as_raw_descriptor());
        }
        if let Some(interrupt_resample_evt) = &self.interrupt_resample_evt {
            rds.push(interrupt_resample_evt.as_raw_descriptor());
        }
        let descriptor = self.msix_config.lock().get_msi_socket();
        rds.push(descriptor);
        rds
    }

    fn assign_irq(
        &mut self,
        irq_evt: &Event,
        irq_resample_evt: &Event,
        irq_num: Option<u32>,
    ) -> Option<(u32, PciInterruptPin)> {
        self.interrupt_evt = Some(irq_evt.try_clone().ok()?);
        self.interrupt_resample_evt = Some(irq_resample_evt.try_clone().ok()?);
        let msix_config_clone = self.msix_config.clone();
        self.device.clone_interrupt(msix_config_clone);

        let gsi = irq_num?;
        self.config.set_irq(gsi as u8, PciInterruptPin::IntA);

        Some((gsi, PciInterruptPin::IntA))
    }

    fn allocate_io_bars(
        &mut self,
        resources: &mut SystemAllocator,
    ) -> std::result::Result<Vec<(u64, u64)>, PciDeviceError> {
        let address = self
            .pci_address
            .expect("allocate_address must be called prior to allocate_io_bars");
        // Pci bridge need one bar for msix
        let mut ranges = Vec::new();
        let bar_addr = resources
            .mmio_allocator(MmioType::Low)
            .allocate_with_align(
                PCI_BRIDGE_BAR_SIZE,
                Alloc::PciBar {
                    bus: address.bus,
                    dev: address.dev,
                    func: address.func,
                    bar: 0,
                },
                "pcie_rootport_bar".to_string(),
                PCI_BRIDGE_BAR_SIZE,
            )
            .map_err(|e| PciDeviceError::IoAllocationFailed(PCI_BRIDGE_BAR_SIZE, e))?;
        let config = PciBarConfiguration::new(
            0,
            PCI_BRIDGE_BAR_SIZE,
            PciBarRegionType::Memory32BitRegion,
            PciBarPrefetchable::NotPrefetchable,
        )
        .set_address(bar_addr);
        self.setting_bar =
            self.config
                .add_pci_bar(config)
                .map_err(|e| PciDeviceError::IoRegistrationFailed(bar_addr, e))? as u8;
        ranges.push((bar_addr, PCI_BRIDGE_BAR_SIZE));

        let msix_cap = MsixCap::new(
            self.setting_bar,
            self.msix_config.lock().num_vectors(),
            BR_MSIX_TABLE_OFFSET as u32,
            self.setting_bar,
            BR_MSIX_PBA_OFFSET as u32,
        );
        let msix_cap_reg = self
            .config
            .add_capability(&msix_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;
        self.msix_cap_reg_idx = Some(msix_cap_reg / 4);

        Ok(ranges)
    }

    fn allocate_device_bars(
        &mut self,
        _resources: &mut SystemAllocator,
    ) -> std::result::Result<Vec<(u64, u64)>, PciDeviceError> {
        Ok(Vec::new())
    }

    fn get_bar_configuration(&self, bar_num: usize) -> Option<PciBarConfiguration> {
        self.config.get_bar_configuration(bar_num)
    }

    fn register_device_capabilities(&mut self) -> std::result::Result<(), PciDeviceError> {
        let caps = self.device.get_caps();
        for cap in caps {
            let cap_reg = self
                .config
                .add_capability(&*cap)
                .map_err(PciDeviceError::CapabilitiesSetup)?;

            self.device.set_capability_reg_idx(cap.id(), cap_reg / 4);
        }

        Ok(())
    }

    fn ioevents(&self) -> Vec<(&Event, u64, Datamatch)> {
        Vec::new()
    }

    fn read_config_register(&self, reg_idx: usize) -> u32 {
        let mut data: u32 = self.config.read_reg(reg_idx);
        if let Some(msix_cap_reg_idx) = self.msix_cap_reg_idx {
            if msix_cap_reg_idx == reg_idx {
                data = self.msix_config.lock().read_msix_capability(data);
                return data;
            }
        }

        self.device.read_config(reg_idx, &mut data);
        data
    }

    fn write_config_register(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if let Some(msix_cap_reg_idx) = self.msix_cap_reg_idx {
            if msix_cap_reg_idx == reg_idx {
                self.msix_config.lock().write_msix_capability(offset, data);
            }
        }

        self.device.write_config(reg_idx, offset, data);

        (&mut self.config).write_reg(reg_idx, offset, data)
    }

    fn read_bar(&mut self, addr: u64, data: &mut [u8]) {
        // The driver is only allowed to do aligned, properly sized access.
        let bar0 = self.config.get_bar_addr(self.setting_bar as usize);
        let offset = addr - bar0;
        if offset < BR_MSIX_PBA_OFFSET {
            self.msix_config
                .lock()
                .read_msix_table(offset - BR_MSIX_TABLE_OFFSET, data);
        } else if BR_MSIX_PBA_OFFSET == offset {
            self.msix_config
                .lock()
                .read_pba_entries(offset - BR_MSIX_PBA_OFFSET, data);
        }
    }

    fn write_bar(&mut self, addr: u64, data: &[u8]) {
        let bar0 = self.config.get_bar_addr(self.setting_bar as usize);
        let offset = addr - bar0;
        if offset < BR_MSIX_PBA_OFFSET {
            self.msix_config
                .lock()
                .write_msix_table(offset - BR_MSIX_TABLE_OFFSET, data);
        } else if BR_MSIX_PBA_OFFSET == offset {
            self.msix_config
                .lock()
                .write_pba_entries(offset - BR_MSIX_PBA_OFFSET, data);
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
enum PcieDevicePortType {
    PcieEndpoint = 0,
    PcieLegacyEndpoint = 1,
    RootPort = 4,
    UpstreamPort = 5,
    DownstreamPort = 6,
    Pcie2PciBridge = 7,
    Pci2PcieBridge = 8,
    RCIntegratedEndpoint = 9,
    RCEventCollector = 0xa,
}

const PCIE_CAP_LEN: usize = 0x3C;

const PCIE_CAP_VERSION: u16 = 0x2;
const PCIE_TYPE_SHIFT: u16 = 0x4;
const PCIE_CAP_SLOT_SHIFT: u16 = 0x8;
const PCIE_CAP_IRQ_NUM_SHIFT: u16 = 0x9;

const PCIE_DEVCAP_RBER: u32 = 0x0000_8000;
const PCIE_LINK_X1: u16 = 0x10;
const PCIE_LINK_2_5GT: u16 = 0x01;

const PCIE_SLTCAP_ABP: u32 = 0x01; // Attention Button Present
const PCIE_SLTCAP_AIP: u32 = 0x08; // Attention Indicator Present
const PCIE_SLTCAP_PIP: u32 = 0x10; // Power Indicator Present
const PCIE_SLTCAP_HPS: u32 = 0x20; // Hot-Plug Surprise
const PCIE_SLTCAP_HPC: u32 = 0x40; // Hot-Plug Capable

const PCIE_SLTCTL_OFFSET: usize = 0x18;
const PCIE_SLTCTL_PIC_OFF: u16 = 0x300;
const PCIE_SLTCTL_AIC_OFF: u16 = 0xC0;
const PCIE_SLTCTL_ABPE: u16 = 0x01;
const PCIE_SLTCTL_PDCE: u16 = 0x08;
const PCIE_SLTCTL_CCIE: u16 = 0x10;
const PCIE_SLTCTL_HPIE: u16 = 0x20;

const PCIE_SLTSTA_OFFSET: usize = 0x1A;
const PCIE_SLTSTA_ABP: u16 = 0x0001;
const PCIE_SLTSTA_PFD: u16 = 0x0002;
const PCIE_SLTSTA_PDC: u16 = 0x0008;
const PCIE_SLTSTA_CC: u16 = 0x0010;
const PCIE_SLTSTA_PDS: u16 = 0x0040;
const PCIE_SLTSTA_DLLSC: u16 = 0x0100;

#[repr(C)]
#[derive(Clone, Copy)]
struct PcieCap {
    _cap_vndr: u8,
    _cap_next: u8,
    pcie_cap: u16,
    dev_cap: u32,
    dev_control: u16,
    dev_status: u16,
    link_cap: u32,
    link_control: u16,
    link_status: u16,
    slot_cap: u32,
    slot_control: u16,
    slot_status: u16,
    root_control: u16,
    root_cap: u16,
    root_status: u32,
    dev_cap_2: u32,
    dev_control_2: u16,
    dev_status_2: u16,
    link_cap_2: u32,
    link_control_2: u16,
    link_status_2: u16,
    slot_cap_2: u32,
    slot_control_2: u16,
    slot_status_2: u16,
}
// It is safe to implement DataInit; all members are simple numbers and any value is valid.
unsafe impl DataInit for PcieCap {}

impl PciCapability for PcieCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::PCIExpress
    }

    fn writable_bits(&self) -> Vec<u32> {
        vec![
            0u32,
            0,
            0xf_ffff,
            0,
            0x3000_0fff,
            0,
            0x11f_1fff,
            0x1f,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]
    }
}

impl PcieCap {
    fn new(device_type: PcieDevicePortType, slot: bool, irq_num: u16) -> Self {
        let mut pcie_cap = PCIE_CAP_VERSION;
        pcie_cap |= (device_type as u16) << PCIE_TYPE_SHIFT;
        if slot {
            pcie_cap |= 1 << PCIE_CAP_SLOT_SHIFT;
        }
        pcie_cap |= irq_num << PCIE_CAP_IRQ_NUM_SHIFT;

        let dev_cap = PCIE_DEVCAP_RBER;
        let link_cap = (PCIE_LINK_X1 | PCIE_LINK_2_5GT) as u32;
        let link_status = PCIE_LINK_X1 | PCIE_LINK_2_5GT;

        let mut slot_cap: u32 = 0;
        let mut slot_control: u16 = 0;
        if slot {
            slot_cap = PCIE_SLTCAP_ABP
                | PCIE_SLTCAP_AIP
                | PCIE_SLTCAP_PIP
                | PCIE_SLTCAP_HPS
                | PCIE_SLTCAP_HPC;
            slot_control = PCIE_SLTCTL_PIC_OFF | PCIE_SLTCTL_AIC_OFF;
        }

        PcieCap {
            _cap_vndr: 0,
            _cap_next: 0,
            pcie_cap,
            dev_cap,
            dev_control: 0,
            dev_status: 0,
            link_cap,
            link_control: 0,
            link_status,
            slot_cap,
            slot_control,
            slot_status: 0,
            root_control: 0,
            root_cap: 0,
            root_status: 0,
            dev_cap_2: 0,
            dev_control_2: 0,
            dev_status_2: 0,
            link_cap_2: 0,
            link_control_2: 0,
            link_status_2: 0,
            slot_cap_2: 0,
            slot_control_2: 0,
            slot_status_2: 0,
        }
    }
}

fn get_word(data: &[u8]) -> Option<u16> {
    if data.len() != 2 {
        return None;
    }

    let value: [u8; 2] = [data[0], data[1]];
    Some(u16::from_le_bytes(value))
}

const PCIE_RP_DID: u16 = 0x3420;
pub struct PcieRootPort {
    pcie_cap_reg_idx: Option<usize>,
    msix_config: Option<Arc<Mutex<MsixConfig>>>,
    slot_control: u16,
    slot_status: u16,
    downstream_device: Option<(PciAddress, Option<HostHotPlugKey>)>,
}

impl PcieRootPort {
    /// Constructs a new PCIE root port
    pub fn new() -> Self {
        PcieRootPort {
            pcie_cap_reg_idx: None,
            msix_config: None,
            slot_control: PCIE_SLTCTL_PIC_OFF | PCIE_SLTCTL_AIC_OFF,
            slot_status: 0,
            downstream_device: None,
        }
    }

    fn read_pcie_cap(&self, offset: usize, data: &mut u32) {
        if offset == PCIE_SLTCTL_OFFSET {
            *data = ((self.slot_status as u32) << 16) | (self.slot_control as u32);
        }
    }

    fn write_pcie_cap(&mut self, offset: usize, data: &[u8]) {
        match offset {
            PCIE_SLTCTL_OFFSET => match get_word(data) {
                Some(v) => {
                    let old_control = self.slot_control;
                    self.slot_control = v;
                    if old_control != v {
                        // send Command completed events
                        self.slot_status |= PCIE_SLTSTA_CC;
                        self.trigger_cc_interrupt();
                    }
                }
                None => warn!("write SLTCTL isn't word, len: {}", data.len()),
            },
            PCIE_SLTSTA_OFFSET => {
                let value = match get_word(data) {
                    Some(v) => v,
                    None => {
                        warn!("write SLTSTA isn't word, len: {}", data.len());
                        return;
                    }
                };
                if value & PCIE_SLTSTA_ABP != 0 {
                    self.slot_status &= !PCIE_SLTSTA_ABP;
                }
                if value & PCIE_SLTSTA_PFD != 0 {
                    self.slot_status &= !PCIE_SLTSTA_PFD;
                }
                if value & PCIE_SLTSTA_PDC != 0 {
                    self.slot_status &= !PCIE_SLTSTA_PDC;
                }
                if value & PCIE_SLTSTA_CC != 0 {
                    self.slot_status &= !PCIE_SLTSTA_CC;
                }
                if value & PCIE_SLTSTA_DLLSC != 0 {
                    self.slot_status &= !PCIE_SLTSTA_DLLSC;
                }
            }
            _ => (),
        }
    }

    fn trigger_interrupt(&self) {
        if let Some(msix_config) = &self.msix_config {
            let mut msix_config = msix_config.lock();
            if msix_config.enabled() {
                msix_config.trigger(0)
            }
        }
    }

    fn trigger_cc_interrupt(&self) {
        if (self.slot_control & PCIE_SLTCTL_CCIE) != 0 && (self.slot_status & PCIE_SLTSTA_CC) != 0 {
            self.trigger_interrupt()
        }
    }

    fn trigger_hp_interrupt(&self) {
        if (self.slot_control & PCIE_SLTCTL_HPIE) != 0
            && (self.slot_status & self.slot_control & (PCIE_SLTCTL_ABPE | PCIE_SLTCTL_PDCE)) != 0
        {
            self.trigger_interrupt()
        }
    }
}

impl PcieDevice for PcieRootPort {
    fn get_device_id(&self) -> u16 {
        PCIE_RP_DID
    }
    fn debug_label(&self) -> String {
        "PcieRootPort".to_string()
    }

    fn clone_interrupt(&mut self, msix_config: Arc<Mutex<MsixConfig>>) {
        self.msix_config = Some(msix_config);
    }

    fn get_caps(&self) -> Vec<Box<dyn PciCapability>> {
        vec![Box::new(PcieCap::new(
            PcieDevicePortType::RootPort,
            true,
            0,
        ))]
    }

    fn set_capability_reg_idx(&mut self, id: PciCapabilityID, reg_idx: usize) {
        if let PciCapabilityID::PCIExpress = id {
            self.pcie_cap_reg_idx = Some(reg_idx)
        }
    }

    fn read_config(&self, reg_idx: usize, data: &mut u32) {
        if let Some(pcie_cap_reg_idx) = self.pcie_cap_reg_idx {
            if reg_idx >= pcie_cap_reg_idx && reg_idx < pcie_cap_reg_idx + (PCIE_CAP_LEN / 4) {
                let offset = (reg_idx - pcie_cap_reg_idx) * 4;
                self.read_pcie_cap(offset, data);
            }
        }
    }

    fn write_config(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if let Some(pcie_cap_reg_idx) = self.pcie_cap_reg_idx {
            if reg_idx >= pcie_cap_reg_idx && reg_idx < pcie_cap_reg_idx + (PCIE_CAP_LEN / 4) {
                let delta = ((reg_idx - pcie_cap_reg_idx) * 4) + offset as usize;
                self.write_pcie_cap(delta, data);
            }
        }
    }
}

impl HotPlugBus for PcieRootPort {
    fn hot_plug(&mut self, addr: PciAddress) {
        match self.downstream_device {
            Some((guest_addr, _)) => {
                if guest_addr != addr {
                    return;
                }
            }
            None => return,
        }

        self.slot_status = self.slot_status | PCIE_SLTSTA_PDS | PCIE_SLTSTA_PDC | PCIE_SLTSTA_ABP;
        self.trigger_hp_interrupt();
    }

    fn hot_unplug(&mut self, addr: PciAddress) {
        match self.downstream_device {
            Some((guest_addr, _)) => {
                if guest_addr != addr {
                    return;
                }
            }
            None => return,
        }

        self.slot_status &= !PCIE_SLTSTA_PDS;
        self.slot_status = self.slot_status | PCIE_SLTSTA_PDC | PCIE_SLTSTA_ABP;
        self.trigger_hp_interrupt();
    }

    fn add_hotplug_device(&mut self, host_key: HostHotPlugKey, guest_addr: PciAddress) {
        self.downstream_device = Some((guest_addr, Some(host_key)))
    }

    fn get_hotplug_device(&self, host_key: HostHotPlugKey) -> Option<PciAddress> {
        if let Some((guest_address, Some(host_info))) = &self.downstream_device {
            match host_info {
                HostHotPlugKey::Vfio { host_addr } => {
                    let saved_addr = *host_addr;
                    match host_key {
                        HostHotPlugKey::Vfio { host_addr } => {
                            if host_addr == saved_addr {
                                return Some(*guest_address);
                            }
                        }
                    }
                }
            }
        }

        None
    }
}
