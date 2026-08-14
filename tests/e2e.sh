#!/bin/zsh
# End-to-end test for timefork on a synthetic repo.
set -e
TF=${TF:-"$(cd "$(dirname "$0")/.." && pwd)/target/debug/timefork"}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/tf-e2e.XXXXXX")
REPO=$WORK/myrepo
mkdir -p $REPO/src $REPO/db
cd $REPO
git init -q .
echo 'fn main() {}' > src/main.rs
echo 'v1' > db/state.db          # untracked "db" — git can't protect this
echo 'target/' > .gitignore
git add -A && git -c user.email=t@t -c user.name=t commit -qm init
ln -s src/main.rs link.rs        # symlink survival check
mkfifo db/pipe.fifo || true      # uncloneable special file check

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; exit 1; }

echo "== init =="
$TF init
[ -d .timefork/trees/000001 ] || fail "baseline tree missing"
pass init

echo "== snap after change =="
echo 'fn main() { println!("v2"); }' > src/main.rs
echo 'v2' > db/state.db
$TF snap -m "manual after v2"
$TF list

echo "== destroy + restore =="
rm -rf src db link.rs            # simulate agent catastrophe (incl. untracked db)
echo junk > junkfile
$TF restore 2 --yes
grep -q 'v2' db/state.db || fail "untracked db not restored"
grep -q 'println' src/main.rs || fail "src not restored"
[ -L link.rs ] || fail "symlink not restored"
[ ! -f junkfile ] || fail "junkfile survived restore"
pass restore

echo "== undo restore (restore the pre-restore snap) =="
PRE_ID=3
$TF restore $PRE_ID --yes
[ -f junkfile ] || fail "undo restore didn't bring junk back"
[ ! -d src ] || fail "undo restore should have removed src"
pass undo-restore
$TF restore 2 --yes   # back to good state

echo "== diff =="
echo extra > newfile.txt
$TF diff 2 live | tee /tmp/tf-diff.out
grep -q '^A newfile.txt' /tmp/tf-diff.out || fail "diff missed added file"
pass diff

echo "== fork from checkpoint =="
$TF fork 2 -n 2
[ -f $WORK/myrepo-fork-2-1/src/main.rs ] || fail "fork 1 missing"
[ -f $WORK/myrepo-fork-2-2/db/state.db ] || fail "fork 2 missing"
[ ! -d $WORK/myrepo-fork-2-1/.timefork ] || fail "fork should not carry .timefork"
# forks are independent: write in fork, verify original untouched
echo 'fork-edit' >> $WORK/myrepo-fork-2-1/src/main.rs
grep -q 'fork-edit' src/main.rs && fail "fork edit leaked into original"
pass fork

echo "== fork live =="
$TF fork live
[ -f $WORK/myrepo-fork-live/newfile.txt ] || fail "live fork missing new file"
pass fork-live

echo "== hook: PreToolUse Bash =="
BEFORE=$($TF list | grep -c '^ ' || true)
echo '{"hook_event_name":"PreToolUse","session_id":"sess-123","cwd":"'$REPO'","tool_name":"Bash","tool_input":{"command":"rm -rf src && npm run build"}}' | $TF hook
$TF list | head -3
$TF list | grep -q 'rm -rf src' || fail "hook snap label missing"
pass hook-bash

echo "== hook: non-mutating tool is ignored =="
COUNT1=$($TF list -n 999 | tail -n +2 | wc -l)
echo '{"hook_event_name":"PreToolUse","cwd":"'$REPO'","tool_name":"Read","tool_input":{"file_path":"x"}}' | $TF hook
COUNT2=$($TF list -n 999 | tail -n +2 | wc -l)
[ "$COUNT1" = "$COUNT2" ] || fail "Read tool caused a snap"
pass hook-ignores-read

echo "== hook: outside a store is a silent no-op =="
echo '{"hook_event_name":"PreToolUse","cwd":"/","tool_name":"Bash","tool_input":{"command":"x"}}' | $TF hook || fail "hook must exit 0"
pass hook-no-store

echo "== hook: garbage stdin still exits 0 =="
echo 'not json' | $TF hook || fail "hook must exit 0 on garbage"
pass hook-garbage

echo "== install-hooks (merge with existing settings) =="
mkdir -p .claude
echo '{"permissions":{"allow":["Bash(ls:*)"]},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}]}}' > .claude/settings.json
$TF install-hooks
python3 -c "
import json; s=json.load(open('.claude/settings.json'))
assert s['permissions']['allow']==['Bash(ls:*)'], 'clobbered permissions'
pre=s['hooks']['PreToolUse']; assert len(pre)==2, pre
assert any('timefork' in h['command'] for e in pre for h in e['hooks'])
assert 'SessionStart' in s['hooks']
print('settings merge OK')
"
$TF install-hooks | grep -q 'already installed' || fail "install-hooks not idempotent"
pass install-hooks

echo "== show =="
$TF show 2

echo "== prune =="
$TF prune --keep 3
LEFT=$(ls .timefork/trees | wc -l | tr -d ' ')
[ "$LEFT" = "3" ] || fail "prune left $LEFT trees, expected 3"
[ -z "$(ls -A .timefork/trash)" ] || fail "trash not cleared"
pass prune

echo "== status =="
$TF status

echo ""
echo "ALL TESTS PASSED"
echo "workdir: $WORK"
