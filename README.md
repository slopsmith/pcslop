# 86slopper

An x86-32 PC emulator for Apple Silicon Macs. The goal is to run Windows XP faster than QEMU's TCG, by running translated code inside a Hypervisor.framework VM where x86 paging maps onto the ARM MMU.

- [Architecture](docs/architecture.md)
- [References](docs/references.md)

## Setup

```sh
scripts/fetch-references.sh   # reference source trees -> references/src (gitignored)
scripts/fetch-docs.sh         # spec PDFs -> references/docs (gitignored)
experiments/hvf-probe/run.sh  # checks HVF, TSO, exit and exception costs on this Mac
```

Licensed under GPL-2.0-or-later.
