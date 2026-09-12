#!/usr/bin/env python3
"""Which unserved routes share the most store machinery — a route-picking aid.

    scripts/deps.py

`scripts/routes.py --todo` says what is left. This says what each of those routes would *cost*,
by walking Go's own call graph from the api4 handler through `app/` to `Store().X().Y()` and
subtracting what a served route already exercises. The output that matters is the last section:
the store methods the most unserved routes are waiting on. That is where a session's work
unlocks a batch rather than a route — the shape that produced the 21-route local-file-backend
commit.

Measured 2026-09-10, on 300 unserved pairs:

    Channel.SaveMember                          18 routes
    Channel.CreateInitialSidebarCategories      17
    Channel.UpdateSidebarCategories             13
    Channel.CreateSidebarCategory               12
    Channel.GetSidebarCategoriesForTeamForUser  12

# What it is not

**Regex, not types.** No interface resolution, no SSA. It therefore has real false negatives —
it reported 157 routes as needing *zero* new store methods, which is not credible (`DELETE
/posts/{post_id}` was among them): it cannot see through the plugin host, the config and
access-control subsystems, or a store call reached via an interface value.

So read the *positive* signals — "these N routes all want `SaveMember`" is trustworthy, because
it found those edges. Do not read the zeroes as "this route is free". A trustworthy version would
use `golang.org/x/tools/go/callgraph` over the SSA, which is buildable because `reference/dump`
already compiles the tree; that is a session of its own and nobody has needed it yet.

**It costs the licensed path, which this port forwards.** Measured 2026-09-12: the three group
syncable routes were ranked joint-most-expensive at 8 new store methods each, and were sequenced
behind two other families on that basis. They needed **zero**. `requireLicense` is the first
statement of all three handlers, so on an unlicensed stack the entire body — the
`GroupSyncable` writes, `Group.TeamMembersToAdd`, the team and channel member writes — is
unreachable, and porting it would have been unverifiable by construction. This walker follows the
call graph straight past the gate.

So before sequencing on a cost from this script, check the handler's first statement. A route
whose family is licence-gated (`api4/group.go` is 20 for 20) costs whatever its *refusal* costs,
which is usually nothing, and the expensive half is a [D-360]-style forward rather than work.
"""
import re, subprocess, collections, pathlib
ROOT = pathlib.Path("/home/niklabh/projekts/mattermost-rs")
API4 = ROOT / "reference/mattermost/server/channels/api4"
APP  = ROOT / "reference/mattermost/server/channels/app"

# --- app-layer call graph -------------------------------------------------
app_body = {}
for f in list(APP.glob("*.go")) + list((APP/"platform").glob("*.go")):
    if f.name.endswith("_test.go"): continue
    src = f.read_text(errors="ignore")
    for m in re.finditer(r"^func \((?:a \*App|a \*App|s \*Server|ps \*PlatformService|ch \*Channels)\) (\w+)\(.*?\{(.*?)^\}", src, re.S|re.M):
        app_body.setdefault(m.group(1), "")
        app_body[m.group(1)] += m.group(2)

STORE = re.compile(r"Store\(\)\.(\w+)\(\)\.(\w+)\(")
APPCALL = re.compile(r"\ba\.(\w+)\(|c\.App\.(\w+)\(|a\.Srv\(\)\.Platform\(\)\.(\w+)\(")

def edges(body):
    apps=set(); stores=set()
    for m in STORE.finditer(body): stores.add(f"{m.group(1)}.{m.group(2)}")
    for m in APPCALL.finditer(body):
        name = m.group(1) or m.group(2) or m.group(3)
        if name and name[0].isupper() or (name and name in app_body): apps.add(name)
    return apps, stores

app_edges = {fn: edges(b) for fn, b in app_body.items()}

def closure(seed_apps, limit=6):
    seen=set(); stores=set(); frontier=set(seed_apps)
    for _ in range(limit):
        nxt=set()
        for fn in frontier:
            if fn in seen or fn not in app_edges: continue
            seen.add(fn)
            a,s = app_edges[fn]; stores |= s; nxt |= a
        frontier = nxt - seen
        if not frontier: break
    return stores

# --- handlers -------------------------------------------------------------
bodies={}
for f in API4.glob("*.go"):
    if f.name.endswith("_test.go"): continue
    src=f.read_text(errors="ignore")
    for m in re.finditer(r"^func (\w+)\(c \*Context, w http\.ResponseWriter, r \*http\.Request\) \{(.*?)^\}", src, re.S|re.M):
        bodies[(f.name,m.group(1))]=m.group(2)

def parse(txt):
    out=[]
    for line in txt.splitlines():
        m=re.match(r"^(GET|PUT|POST|DELETE|PATCH|HEAD)\s+(\S+)\s+(\w+) \((\S+)\)", line)
        if m: out.append((m.group(1),m.group(2),m.group(3),m.group(4)))
    return out

todo=parse(subprocess.run(["scripts/routes.py","--todo"],cwd=ROOT,capture_output=True,text=True).stdout)
served=parse(subprocess.run(["scripts/routes.py","--served"],cwd=ROOT,capture_output=True,text=True).stdout)

def route_stores(rs):
    out={}
    for meth,path,h,fn in rs:
        b=bodies.get((fn,h),"")
        a,s=edges(b)
        out[(meth,path,fn)] = s | closure(a)
    return out

todo_s = route_stores(todo)
have_s = set().union(*route_stores(served).values()) if served else set()

need=collections.Counter()
for r,s in todo_s.items():
    for x in s-have_s: need[x]+=1

print(f"unserved routes: {len(todo_s)}")
print(f"distinct Store.* methods they need: {len(set().union(*todo_s.values()))}")
print(f"   already exercised by a served route: {len(set().union(*todo_s.values()) & have_s)}")
print(f"   NEW store methods still to port:     {len(need)}\n")

sizes = sorted((len(s-have_s), m, p, f) for (m,p,f),s in todo_s.items())
print("=== cheapest unserved routes (new store methods each needs) ===")
for n,m,p,f in sizes[:25]:
    print(f"  {n:3}  {m:6} {p}")
print("\n=== most expensive ===")
for n,m,p,f in sizes[-10:]:
    print(f"  {n:3}  {m:6} {p}")

print("\n=== the most-shared NEW store methods ===")
for fn,n in need.most_common(20):
    print(f"  {n:4} routes need  {fn}")

hist=collections.Counter(n for n,_,_,_ in sizes)
print("\n=== how many routes need N new store methods ===")
for k in sorted(hist)[:12]:
    print(f"  {k:3} new methods : {hist[k]:3} routes")
