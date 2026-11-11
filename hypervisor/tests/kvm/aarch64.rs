// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use hypervisor::kvm::*;
use hypervisor::*;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

#[test]
fn readonly_cache_maintenance_and_data_abort() {
    use base::MemoryMappingBuilder;

    for (instruction, address, skip) in [
        (0xd50b7522u32, 0x80020000, true),  // ic ivau, x2
        (0xa9000440u32, 0x80020000, false), // stp x0, x1, [x2]
        (0xa9000440u32, 0x80030000, false),
    ] {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0x80000000), 0x10000)]).unwrap();
        gm.write_obj_at_addr(instruction, GuestAddress(0x80000000))
            .unwrap();
        for address in [0x80000004, 0x80000a00] {
            gm.write_obj_at_addr(0xf9400060u32, GuestAddress(address))
                .unwrap(); // ldr x0, [x3]
        }
        let vm = KvmVm::new(&kvm, gm, Default::default()).unwrap();
        let slot = vm
            .add_memory_region(
                GuestAddress(0x80020000),
                Box::new(MemoryMappingBuilder::new(0x1000).build().unwrap()),
                true,
                false,
                MemCacheType::CacheCoherent,
            )
            .unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        vcpu.init(&[]).unwrap();
        vcpu.set_one_reg(VcpuRegAArch64::Pc, 0x80000000).unwrap();
        vcpu.set_one_reg(VcpuRegAArch64::Pstate, 0x3c5).unwrap();
        vcpu.set_one_reg(VcpuRegAArch64::X(2), address).unwrap();
        vcpu.set_one_reg(VcpuRegAArch64::X(3), 0x90000000).unwrap();
        vcpu.set_one_reg(
            VcpuRegAArch64::System(aarch64_sys_reg::VBAR_EL1),
            0x80000800,
        )
        .unwrap();
        assert_eq!(vcpu.run().unwrap_err().errno(), libc::EAGAIN);
        if skip {
            assert_eq!(vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap(), 0x80000004);
        }
        // Pending exception state is applied on the next KVM_RUN.
        assert!(matches!(vcpu.run().unwrap(), VcpuExit::Mmio));
        if !skip {
            assert_eq!(vcpu.get_one_reg(VcpuRegAArch64::Pc).unwrap(), 0x80000a00);
            let esr = vcpu
                .get_one_reg(VcpuRegAArch64::System(aarch64_sys_reg::ESR_EL1))
                .unwrap();
            assert_eq!(esr & 0x3f, 0x10); // Synchronous external abort.
        }
        vm.remove_memory_region(slot).unwrap();
    }
}

#[test]
fn set_gsi_routing() {
    let kvm = Kvm::new().unwrap();
    let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
    let vm = KvmVm::new(&kvm, gm, Default::default()).unwrap();
    vm.create_irq_chip().unwrap();
    vm.set_gsi_routing(&[]).unwrap();
    vm.set_gsi_routing(&[IrqRoute {
        gsi: 1,
        source: IrqSource::Irqchip {
            chip: IrqSourceChip::Gic,
            pin: 3,
        },
    }])
    .unwrap();
    vm.set_gsi_routing(&[IrqRoute {
        gsi: 1,
        source: IrqSource::Msi {
            address: 0xf000000,
            data: 0xa0,
            pci_address: Default::default(),
        },
    }])
    .unwrap();
    vm.set_gsi_routing(&[
        IrqRoute {
            gsi: 1,
            source: IrqSource::Irqchip {
                chip: IrqSourceChip::Gic,
                pin: 3,
            },
        },
        IrqRoute {
            gsi: 2,
            source: IrqSource::Msi {
                address: 0xf000000,
                data: 0xa0,
                pci_address: Default::default(),
            },
        },
    ])
    .unwrap();
}
