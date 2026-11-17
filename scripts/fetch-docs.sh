#!/bin/sh
# Downloads spec PDFs into references/docs (gitignored). Most are not redistributable.
set -u
dest="$(cd "$(dirname "$0")/.." && pwd)/references/docs"
mkdir -p "$dest"
ua="Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"
wb=http://web.archive.org/web
bs=http://www.bitsavers.org
mit=https://pdos.csail.mit.edu/6.828/2018/readings
pintos=https://web.stanford.edu/class/cs140/projects/pintos/specs

get() {
	if [ -s "$dest/$1" ]; then echo "have $1"; return; fi
	echo "=== $1"
	curl -fL --progress-bar -A "$ua" -o "$dest/$1" "$2" || { echo "FAIL $1"; rm -f "$dest/$1"; }
}

get intel-sdm-combined-325462.pdf https://cdrdv2.intel.com/v1/dl/getContent/671200
get intel-optimization-reference-248966.pdf https://cdrdv2.intel.com/v1/dl/getContent/671488
get i386-programmers-reference-1986.pdf $bs/components/intel/80386/230985-001_80386_Programmers_Reference_Manual_1986.pdf
get i486-programmers-reference-1990.pdf $bs/components/intel/80486/i486_Processor_Programmers_Reference_Manual_1990.pdf
get intel-440fx-pciset-82441fx-82442fx-29054901.pdf $wb/2010/http://download.intel.com/design/chipsets/datashts/29054901.pdf
get intel-440bx-82443bx-29063301.pdf $wb/2010/http://download.intel.com/design/chipsets/datashts/29063301.pdf
get intel-piix3-82371sb-29055002.pdf $wb/2005/http://www.intel.com/design/chipsets/datashts/29055002.pdf
get intel-piix4-82371ab-29056201.pdf $wb/2005/http://www.intel.com/design/chipsets/datashts/29056201.pdf
get intel-mp-spec-1.4-24201606.pdf $wb/2010/http://download.intel.com/design/archives/processors/pro/docs/24201606.pdf
get intel-82093aa-ioapic-29056601.pdf $wb/2010/http://download.intel.com/design/chipsets/datashts/29056601.pdf
get acpi-1.0b.pdf $wb/2010id_/http://www.acpi.info/DOWNLOADS/ACPIspec10b.pdf
get acpi-2.0c.pdf $wb/2010id_/http://www.acpi.info/DOWNLOADS/ACPIspec-2-0c.pdf
get intel-8259a-pic.pdf $mit/hardware/8259A.pdf
get intel-82c54-pit.pdf $mit/hardware/82C54.pdf
get intel-8237a-dma.pdf $mit/hardware/8237A.pdf
get motorola-mc146818a-rtc.pdf $pintos/mc146818a.pdf
get st-m48t86-rtc-mc146818-compatible.pdf $mit/hardware/M48T86.pdf
get intel-8742-upi-42-datasheet-rochester.pdf https://resources.ampheo.com/static/datasheets/rochester-electronics-llc/d8742.pdf
get intel-upi-41a-users-manual-1980.pdf $bs/components/intel/8048/1980_UPI_41A_Users_Manual.pdf
get ns-pc16550d-uart.pdf $pintos/pc16550d.pdf
for p in a b c d e f; do
	get rbil-inter61$p.zip http://www.cs.cmu.edu/~ralf/interrupt-list/inter61$p.zip
done
get phoenix-bios-boot-spec-1.01.pdf $wb/2010id_/http://www.phoenix.com:80/docs/specsbbs101.pdf
get el-torito-bootable-cdrom-1.0.pdf $mit/boot-cdrom.pdf
get pci-local-bus-spec-2.2-199812.pdf $bs/pdf/pciSIG/PCI_Local_Bus_Specification_2.2_199812.pdf
get sff-8038i-ide-busmaster.pdf $mit/hardware/IDE-BusMaster.pdf
get t13-1410d-ata-atapi-6-r3a.pdf $mit/hardware/ATA-d1410r3a.pdf
t13=$wb/2010id_/http://www.t13.org:80/Documents/UploadedDocuments/docs2007
get t13-1532d-ata-atapi-7-vol1-r4b.pdf $t13/D1532v1r4b-AT_Attachment_with_Packet_Interface_-_7_Volume_1.pdf
get t13-1532d-ata-atapi-7-vol2-r4b.pdf $t13/D1532v2r4b-AT_Attachment_with_Packet_Interface_-_7_Volume_2.pdf
get sff-8020i-atapi-cdrom-rev2.6.pdf $wb/2020id_/https://www.mikrocontroller.net/attachment/7769/8020rev26.pdf
get vesa-vbe-3.0.pdf $mit/hardware/vbe3.pdf
get arm-ddi0487-M.d-a-profile-arm-arm.pdf https://documentation-service.arm.com/static/6abbec2a6db85c5e6614a4a1
get amd-R3xx_3D_Registers.pdf https://www.x.org/docs/AMD/old/R3xx_3D_Registers.pdf
get amd-R5xx_Acceleration_v1.5.pdf https://www.x.org/docs/AMD/old/R5xx_Acceleration_v1.5.pdf
