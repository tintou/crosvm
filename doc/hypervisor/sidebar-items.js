initSidebarItems({"enum":[["Datamatch","Used in `Vm::register_ioevent` to indicate a size and optionally value to match."],["DeviceKind","A device type to create with `Vm.create_device`."],["IoEventAddress","An address either in programmable I/O space or in memory mapped I/O space."],["IrqSource","A source of IRQs in an `IrqRoute`."],["IrqSourceChip","The source chip of an `IrqSource`"],["MPState","The MPState represents the state of a processor."],["VcpuExit","A reason why a VCPU exited. One of these returns every time `Vcpu::run` is called."]],"mod":[["caps",""],["kvm",""],["x86_64",""]],"struct":[["ClockState","The state of the paravirtual clock."],["IrqRoute","A single route for an IRQ."],["VcpuRunHandle","A handle returned by a `Vcpu` to be used with `Vcpu::run` to execute a virtual machine’s VCPU."],["VcpuRunHandleFingerprint","A unique fingerprint for a particular `VcpuRunHandle`, used in `Vcpu` impls to ensure the `VcpuRunHandle ` they receive is the same one that was returned from `take_run_handle`."]],"trait":[["Hypervisor","A trait for checking hypervisor capabilities."],["Vcpu","A virtual CPU holding a virtualized hardware thread’s state, such as registers and interrupt state, which may be used to execute virtual machines."],["Vm","A wrapper for using a VM and getting/setting its state."]],"type":[["MemSlot","An index in the list of guest-mapped memory regions."]]});