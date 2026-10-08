#!/usr/bin/env bash
# Trace one VM's host round trips for a few seconds (read-only: uprobes and
# tracepoints on the running backend, conduit-venus and QEMU; nothing is
# stopped or changed). docs/research/host-roundtrip-latency.md.
#
#   host/latency/capture.sh [--light] VM SECONDS OUT
#
# Writes OUT (events) and OUT.threads (tid and name of every thread of the
# three processes, taken when the capture ends). Analyse with
# host/latency/analyze.py OUT. Needs bpftrace and sudo. --light keeps three
# uprobes (Venus::dispatch, the vkr timeline write, the fence callback):
# under 15 us of probe cost per round trip, against about 30 for the full set.
set -euo pipefail
light=0
if [ "${1:-}" = --light ]; then light=1; shift; fi
[ $# -eq 3 ] || { sed -n '6,13p' "$0" >&2; exit 2; }
vm=$1 secs=$2 out=$3
[ "$secs" -le 30 ] || { echo "keep a capture at 30 s or less" >&2; exit 2; }

BP=$(pgrep -x conduit-backend | head -1)
VP=$(pgrep -x conduit-venus | head -1)
QP=$(pgrep -f "qemu-system-x86_64.*guest=$vm" | head -1)
[ -n "$BP" ] && [ -n "$VP" ] && [ -n "$QP" ] || { echo "backend, conduit-venus or QEMU for $vm not running" >&2; exit 1; }
B=$(readlink -f /proc/$BP/exe 2>/dev/null || sudo readlink -f /proc/$BP/exe)
V=$(readlink -f /proc/$VP/exe 2>/dev/null || sudo readlink -f /proc/$VP/exe)
L=$(sudo awk '!f && /libvirglrenderer/ {print $6; f = 1}' /proc/$VP/maps)
sym() { nm "$1" | awk -v s="$2" '!f && $3 ~ s {print $3; f = 1}'; }
DISP=$(sym "$B" '^_ZN6device5venus5Venus8dispatch17')
SUB=$(sym "$B" 'IpcClient.*Renderer.*6submit17')
SF=$(sym "$B" 'IpcClient.*Renderer.*13submit_fenced17')
CF=$(sym "$B" 'IpcClient.*Renderer.*12create_fence17')
DC=$(sym "$B" '^_ZN15conduit_backend19deliver_completions17')
RC=$(sym "$B" '^_ZN15conduit_backend13return_chains17')
SU=$(sym "$B" 'VringRwLock.*17signal_used_queue17')
VSUB=$(sym "$V" 'Virgl.*Renderer.*6submit17')
VCF=$(sym "$V" 'Virgl.*Renderer.*12create_fence17')
WCF=$(sym "$V" 'virgl19write_context_fence17')
# The NVIDIA interrupt lines: when the GPU said it was done.
NVIRQ=$(awk -F: '/nvidia/ {gsub(/ /,"",$1); printf "%sargs.irq == %s", sep, $1; sep=" || "}' /proc/interrupts)

up() { # uprobe/uretprobe lines for a symbol that exists
    [ -n "$3" ] || return 0
    printf '%s:%s:"%s" /pid == %s/ { %s }\n' "$1" "$2" "$3" "$4" "$5"
}
{
cat <<BT
config = { max_strlen = 16 }
rawtracepoint:sched_wakeup, rawtracepoint:sched_wakeup_new {
  \$p = (struct task_struct *)arg0; \$g = \$p->tgid;
  if (\$g == $BP || \$g == $VP || (\$g == $QP && strncmp(\$p->comm, "CPU", 3) == 0)) {
    \$c = \$p->thread_info.cpu;
    printf("WK %llu %d %d %d %d %s\n", nsecs, \$p->pid, \$c, @cst[\$c], tid, comm);
  }
}
rawtracepoint:sched_switch {
  \$n = (struct task_struct *)arg2; \$g = \$n->tgid;
  if (\$g == $BP || \$g == $VP) { printf("RN %llu %d %d\n", nsecs, \$n->pid, cpu); }
}
tracepoint:power:cpu_idle { @cst[args.cpu_id] = args.state == 4294967295 ? 0 : args.state + 1; }
tracepoint:kvm:kvm_msi_set_irq /pid == $BP/ { printf("MSI %llu %d %llu\n", nsecs, tid, args.address); }
tracepoint:kvm:kvm_set_irq /pid == $QP && tid == $QP && args.level == 1/ { printf("IRQ %llu %d %u\n", nsecs, tid, args.gsi); }
BT
[ -n "$NVIRQ" ] && echo "tracepoint:irq:irq_handler_entry /$NVIRQ/ { printf(\"NI %llu %d %d\n\", nsecs, tid, cpu); }"
up uprobe "$B" "$DISP" "$BP" '$h = arg1; printf("D0 %llu %d %u %u %llu %u %u\n", nsecs, tid, *(uint32*)$h, *(uint32*)($h+4), *(uint64*)($h+8), *(uint32*)($h+16), *(uint8*)($h+20));'
up uprobe "$L" render_context_update_timeline "$VP" 'printf("T %llu %d %u %u\n", nsecs, tid, arg1, arg2);'
up uprobe "$V" "$WCF" "$VP" 'printf("WF %llu %d %u %u %llu\n", nsecs, tid, arg1, arg2, arg3);'
if [ $light = 0 ]; then
up uretprobe "$B" "$DISP" "$BP" 'printf("D1 %llu %d\n", nsecs, tid);'
up uprobe "$B" "$SUB" "$BP" 'printf("S0 %llu %d\n", nsecs, tid);'
up uretprobe "$B" "$SUB" "$BP" 'printf("S1 %llu %d\n", nsecs, tid);'
up uprobe "$B" "$SF" "$BP" 'printf("S0 %llu %d\n", nsecs, tid);'
up uretprobe "$B" "$SF" "$BP" 'printf("F1 %llu %d\n", nsecs, tid);'
up uprobe "$B" "$CF" "$BP" 'printf("F0 %llu %d\n", nsecs, tid);'
up uretprobe "$B" "$CF" "$BP" 'printf("F1 %llu %d\n", nsecs, tid);'
up uprobe "$B" "$DC" "$BP" 'printf("C0 %llu %d\n", nsecs, tid);'
up uprobe "$B" "$RC" "$BP" 'printf("C0 %llu %d\n", nsecs, tid);'
up uprobe "$B" "$SU" "$BP" 'printf("U0 %llu %d\n", nsecs, tid);'
up uprobe "$V" "$VSUB" "$VP" 'printf("VS0 %llu %d\n", nsecs, tid);'
up uretprobe "$V" "$VSUB" "$VP" 'printf("VS1 %llu %d\n", nsecs, tid);'
up uprobe "$V" "$VCF" "$VP" 'printf("VF0 %llu %d %u %u %llu\n", nsecs, tid, arg2, arg3, arg4);'
up uretprobe "$V" "$VCF" "$VP" 'printf("VF1 %llu %d\n", nsecs, tid);'
up uprobe "$L" render_context_dispatch_submit_cmd "$VP" 'printf("RC0 %llu %d\n", nsecs, tid);'
up uprobe "$L" vkr_dispatch_vkQueueSubmit "$VP" 'printf("Q0 %llu %d\n", nsecs, tid);'
up uretprobe "$L" vkr_dispatch_vkQueueSubmit "$VP" 'printf("Q1 %llu %d\n", nsecs, tid);'
up uprobe "$L" vkr_queue_sync_submit "$VP" 'printf("Y0 %llu %d %u %llu\n", nsecs, tid, arg2, arg3);'
up uretprobe "$L" vkr_queue_sync_submit "$VP" 'printf("Y1 %llu %d\n", nsecs, tid);'
fi
} > "$out.bt"
sudo timeout "$secs" bpftrace -q "$out.bt" > "$out" || true
for p in $BP $VP; do
    for t in /proc/$p/task/*; do echo "$(basename "$t") $(cat "$t/comm")"; done
done > "$out.threads"
echo "$(grep -c '^D0' "$out") dispatches, $(wc -l < "$out") events in $out"
