# Trusted Platform Module

Crosvm can expose a virtio TPM device backed by either the ChromeOS vTPM daemon or
[swtpm]. Build Crosvm with the `vtpm` feature to enable these options.

To use swtpm, start it with a Unix control socket:

```sh
mkdir -p /var/lib/swtpm/vm/state
swtpm socket \
    --tpm2 \
    --tpmstate dir=/var/lib/swtpm/vm/state \
    --ctrl type=unixio,path=/run/swtpm-vm.sock
```

Pass that control socket to Crosvm:

```sh
crosvm run --swtpm /run/swtpm-vm.sock # usual crosvm arguments
```

The guest kernel must include the virtio TPM driver for virtio device ID 62. This
driver is available in the ChromiumOS Linux tree but is not currently in mainline
Linux.

The `--vtpm-proxy` and `--swtpm` options are mutually exclusive.

[swtpm]: https://github.com/stefanberger/swtpm
