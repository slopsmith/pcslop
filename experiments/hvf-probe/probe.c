#include <Hypervisor/Hypervisor.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define CHECK(x) do { hv_return_t r_ = (x); if (r_ != HV_SUCCESS) { fprintf(stderr, "%s:%d %s = 0x%x\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

#define MEM_SIZE (2u << 20)
#define CODE_OFF 0x0000
#define VEC_OFF 0x1000
#define L1_OFF 0x10000
#define L2_OFF 0x11000
#define L3_OFF 0x12000
#define DATA_OFF 0x20000
#define FLAG_OFF 0x20080

extern char guest_start[], guest_end[], guest_vec[], guest_vec_end[];

__asm__(
	".p2align 2\n"
	".globl _guest_start\n_guest_start:\n"
	"  cmp x5, #1\n  b.eq 10f\n  cmp x5, #2\n  b.eq 20f\n  cmp x5, #3\n  b.eq 30f\n"
	"0: hvc #0\n  b 0b\n"
	"10: mov x2, x0\n11: svc #0\n  subs x2, x2, #1\n  b.ne 11b\n  hvc #0\n"
	"20: mov x1, #1\n21: str x1, [x6]\n  str x1, [x7]\n  add x1, x1, #1\n  cmp x1, x0\n  b.ne 21b\n  hvc #0\n"
	"30: mov x3, #0\n  mov x4, #0\n31: ldr x1, [x7]\n  ldr x2, [x6]\n  cmp x2, x1\n  cinc x3, x3, lo\n"
	"  add x4, x4, #1\n  cmp x4, x0\n  b.ne 31b\n  hvc #0\n"
	".globl _guest_end\n_guest_end:\n"
	".p2align 11\n"
	".globl _guest_vec\n_guest_vec:\n"
	"  .rept 4\n  eret\n  .p2align 7\n  .endr\n"
	"  .rept 4\n  eret\n  .p2align 7\n  .endr\n"
	".globl _guest_vec_end\n_guest_vec_end:\n");

static uint8_t *mem;
static int tso;

static double ticks_to_ns(uint64_t t) {
	mach_timebase_info_data_t tb;
	mach_timebase_info(&tb);
	return (double)t * tb.numer / tb.denom;
}

static void setup_pagetables(void) {
	uint64_t *l1 = (uint64_t *)(mem + L1_OFF), *l2 = (uint64_t *)(mem + L2_OFF), *l3 = (uint64_t *)(mem + L3_OFF);
	l1[0] = L2_OFF | 3;
	l2[0] = L3_OFF | 3;
	for (uint64_t i = 0; i < 512; i++)
		l3[i] = (i << 12) | (1 << 10) | (3 << 8) | 3;
}

static hv_vcpu_t make_vcpu(hv_vcpu_exit_t **ex, uint64_t mode, uint64_t count) {
	hv_vcpu_t v;
	CHECK(hv_vcpu_create(&v, ex, NULL));
	CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_MAIR_EL1, 0xff));
	CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_TCR_EL1, 25 | (1 << 8) | (1 << 10) | (3 << 12) | (1ull << 23)));
	CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_TTBR0_EL1, L1_OFF));
	CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_VBAR_EL1, VEC_OFF));
	CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_SCTLR_EL1, 0x30d00800 | 1 | 4 | (1 << 12)));
	CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_ACTLR_EL1, tso ? 2 : 0));
	uint64_t actlr;
	CHECK(hv_vcpu_get_sys_reg(v, HV_SYS_REG_ACTLR_EL1, &actlr));
	if ((actlr & 2) != (tso ? 2u : 0u)) {
		fprintf(stderr, "ACTLR_EL1 readback mismatch: 0x%llx\n", actlr);
		exit(1);
	}
	CHECK(hv_vcpu_set_reg(v, HV_REG_CPSR, 0x3c5));
	CHECK(hv_vcpu_set_reg(v, HV_REG_PC, CODE_OFF));
	CHECK(hv_vcpu_set_reg(v, HV_REG_X0, count));
	CHECK(hv_vcpu_set_reg(v, HV_REG_X5, mode));
	CHECK(hv_vcpu_set_reg(v, HV_REG_X6, DATA_OFF));
	CHECK(hv_vcpu_set_reg(v, HV_REG_X7, FLAG_OFF));
	return v;
}

static void run_until_hvc(hv_vcpu_t v, hv_vcpu_exit_t *ex) {
	CHECK(hv_vcpu_run(v));
	uint32_t ec = (uint32_t)(ex->exception.syndrome >> 26);
	if (ex->reason != HV_EXIT_REASON_EXCEPTION || ec != 0x16) {
		uint64_t pc;
		hv_vcpu_get_reg(v, HV_REG_PC, &pc);
		fprintf(stderr, "unexpected exit reason=%d ec=0x%x syndrome=0x%llx va=0x%llx pc=0x%llx\n", ex->reason, ec,
			ex->exception.syndrome, ex->exception.virtual_address, pc);
		exit(1);
	}
}

struct litmus_arg { uint64_t mode, count, violations; };

static void *litmus_thread(void *p) {
	struct litmus_arg *a = p;
	hv_vcpu_exit_t *ex;
	hv_vcpu_t v = make_vcpu(&ex, a->mode, a->count);
	run_until_hvc(v, ex);
	CHECK(hv_vcpu_get_reg(v, HV_REG_X3, &a->violations));
	hv_vcpu_destroy(v);
	return NULL;
}

int main(int argc, char **argv) {
	tso = argc > 1 && strcmp(argv[1], "tso") == 0;
	uint32_t max_ipa, def_ipa;
	bool el2;
	CHECK(hv_vm_config_get_max_ipa_size(&max_ipa));
	CHECK(hv_vm_config_get_default_ipa_size(&def_ipa));
	CHECK(hv_vm_config_get_el2_supported(&el2));
	printf("max IPA %u bits, default %u bits, EL2 supported: %d\n", max_ipa, def_ipa, el2);

	CHECK(hv_vm_create(NULL));
	mem = mmap(NULL, MEM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
	memcpy(mem + CODE_OFF, guest_start, guest_end - guest_start);
	memcpy(mem + VEC_OFF, guest_vec, guest_vec_end - guest_vec);
	setup_pagetables();
	CHECK(hv_vm_map(mem, 0, MEM_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));

	hv_vcpu_exit_t *ex;
	hv_vcpu_t v = make_vcpu(&ex, 0, 0);
	uint64_t mmfr0;
	CHECK(hv_vcpu_get_sys_reg(v, HV_SYS_REG_ID_AA64MMFR0_EL1, &mmfr0));
	printf("ID_AA64MMFR0_EL1 = 0x%llx (TGran4 field = %llu, 0 means 4K supported)\n", mmfr0, (mmfr0 >> 28) & 0xf);

	const int exits = 200000;
	run_until_hvc(v, ex);
	uint64_t t0 = mach_absolute_time();
	for (int i = 0; i < exits; i++)
		run_until_hvc(v, ex);
	printf("VM exit round trip (hvc): %.0f ns\n", ticks_to_ns(mach_absolute_time() - t0) / exits);
	hv_vcpu_destroy(v);

	const uint64_t svcs = 20000000;
	v = make_vcpu(&ex, 1, svcs);
	t0 = mach_absolute_time();
	run_until_hvc(v, ex);
	printf("in-guest svc + eret (no VM exit): %.2f ns\n", ticks_to_ns(mach_absolute_time() - t0) / svcs);
	hv_vcpu_destroy(v);

	struct litmus_arg w = {2, 200000000, 0}, r = {3, 400000000, 0};
	pthread_t tw, tr;
	pthread_create(&tr, NULL, litmus_thread, &r);
	pthread_create(&tw, NULL, litmus_thread, &w);
	pthread_join(tw, NULL);
	pthread_join(tr, NULL);
	printf("message-passing litmus (%s): %llu reorderings observed in %llu reads\n", tso ? "TSO" : "weak",
		r.violations, r.count);
	return 0;
}
