#!/usr/bin/env python3
"""Run a plan of `pkg`-suite mutations, each against its own crate, with a time and memory cap.

    scripts/mutate-pkg-plan.py <plan-file> [name-substring]

The plan is `scripts/mutate-batch.sh`'s tab-separated format with the sixth field naming the
**package** (MUTATE_PACKAGE) rather than a test filter:

    name<TAB>file<TAB>from<TAB>to<TAB>pkg<TAB>package

`\\n` and `\\\\` in `from`/`to` decode to a newline and a backslash. Why not mutate-batch.sh: that
takes the stack lock, which a generic crate's suite never needs, and it has no per-mutation
timeout — a parser mutant that loops forever would hang the batch, and one that allocates without
bound has taken the whole session down before (hence the address-space cap). A mutation that
times out is reported as CAUGHT (timeout): the suite did see the change.
"""
import os
import resource
import subprocess
import sys

TIMEOUT = 600
ADDRESS_SPACE = 16_000_000 * 1024


def decode(s):
    out, i = [], 0
    while i < len(s):
        if s[i] == '\\' and i + 1 < len(s) and s[i + 1] in 'n\\':
            out.append('\n' if s[i + 1] == 'n' else '\\')
            i += 2
        else:
            out.append(s[i])
            i += 1
    return ''.join(out)


def cap():
    resource.setrlimit(resource.RLIMIT_AS, (ADDRESS_SPACE, ADDRESS_SPACE))


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    plan, only = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else '')
    lines = [l.rstrip('\n') for l in open(plan, encoding='utf-8')]
    rows = [l.split('\t') for l in lines if l and not l.startswith('#')]
    for r in rows:
        body = open(os.path.join(root, r[1]), encoding='utf-8').read()
        if body.count(decode(r[2])) != 1:
            sys.exit(f'plan: {r[0]}: pattern occurs {body.count(decode(r[2]))} times in {r[1]}')
    tally = {'run': 0, 'caught': 0, 'survived': [], 'fault': [], 'controls_survived': 0}
    for name, file, frm, to, suite, package in rows:
        if only not in name:
            continue
        env = dict(os.environ, MUTATE_PACKAGE=package)
        try:
            p = subprocess.run([os.path.join(root, 'scripts/mutate.sh'), name, file, decode(frm),
                                decode(to), suite], cwd=root, env=env, capture_output=True,
                               text=True, timeout=TIMEOUT, preexec_fn=cap)
            verdict = (p.stdout.strip().splitlines() or ['(no output)'])[-1]
        except subprocess.TimeoutExpired:
            verdict = f'{name}: CAUGHT (timeout after {TIMEOUT}s)'
        # mutate.sh restores on TERM; make sure nothing is left applied either way.
        subprocess.run(['git', 'checkout', '--', file], cwd=root)
        print(verdict, flush=True)
        tally['run'] += 1
        control = name.startswith('control')
        if 'SURVIVED' in verdict:
            if control:
                tally['controls_survived'] += 1
            else:
                tally['survived'].append(name)
        elif 'CAUGHT' in verdict:
            tally['caught'] += 1
        else:
            tally['fault'].append(name)
    print(f"TALLY: {tally['run']} run, {tally['caught']} caught, "
          f"{tally['controls_survived']} controls survived; survivors {tally['survived']}; "
          f"faults {tally['fault']}")


if __name__ == '__main__':
    main()
