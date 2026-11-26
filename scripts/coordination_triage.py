#!/usr/bin/env python3
"""Read-only upstream PR triage. Never checks out or executes candidate code."""
from __future__ import annotations
import argparse
import datetime as dt
import itertools
import json
import os
from pathlib import Path
import re
import subprocess
from collections import defaultdict

STOP = {"fix","the","and","for","with","from","when","gpu","shader","shaders","hle","support","add","implement","improve","update"}

def git(repo, *args, check=True):
    p = subprocess.run(["git", *args], cwd=repo, text=True, encoding="utf-8",
                       errors="replace", stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and p.returncode:
        raise RuntimeError(f"git {' '.join(args)} failed ({p.returncode}): {p.stderr.strip()}")
    return p

def merge_tree(repo, base, head):
    p = git(repo, "merge-tree", "--write-tree", "--name-only", "--no-messages", base, head, check=False)
    lines = [x.strip() for x in p.stdout.splitlines() if x.strip()]
    tree = lines[0] if lines and re.fullmatch(r"[0-9a-f]{40,64}", lines[0]) else None
    files = lines[1:] if tree else lines
    if p.returncode == 0: return {"status":"clean","tree":tree,"conflicts":[]}
    if p.returncode == 1: return {"status":"conflict","tree":tree,"conflicts":files}
    return {"status":"error","tree":tree,"conflicts":[],"error":p.stderr.strip()[-1000:]}

def patch_id(repo, base, head):
    diff = git(repo, "diff", "--no-ext-diff", "--no-textconv", "--no-color", "--binary", f"{base}...{head}").stdout
    p = subprocess.run(["git","patch-id","--stable"], input=diff, cwd=repo, text=True,
                       encoding="utf-8", errors="replace", stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode: raise RuntimeError(f"git patch-id failed: {p.stderr.strip()}")
    fields = p.stdout.split()
    return fields[0] if fields else None

def ancestor(repo, a, b):
    return git(repo, "merge-base", "--is-ancestor", a, b, check=False).returncode == 0

def issues(pr):
    out=set()
    for x in pr.get("closingIssuesReferences") or []:
        try: out.add(int(x["number"]))
        except (KeyError,TypeError,ValueError): pass
    return out

def words(title):
    return set(re.findall(r"[a-z][a-z_0-9]{2,}", title.lower())) - STOP

def overlap_hints(items):
    by_file=defaultdict(set); by_num={int(x["number"]):x for x in items}
    for x in items:
        for path in x.get("changed_files",[]): by_file[path].add(int(x["number"]))
    pairs=set()
    for nums in by_file.values(): pairs.update(itertools.combinations(sorted(nums),2))
    out=[]
    for a,b in sorted(pairs):
        x,y=by_num[a],by_num[b]
        common=sorted(set(x.get("changed_files",[])) & set(y.get("changed_files",[])))
        shared=sorted(issues(x) & issues(y))
        wx,wy=words(x.get("title","")),words(y.get("title",""))
        sim=len(wx & wy)/max(1,len(wx | wy))
        if shared or sim>=0.70:
            out.append({"prs":[a,b],"reason":"shared closing issue" if shared else "similar title + shared files",
                        "issues":shared,"files":common[:20],"title_similarity":round(sim,3)})
    return out

def base_state(repo, base_sha, main_sha, cache):
    if not base_sha:
        return {"base_status":"unknown","base_stale":None,"base_behind_by":None}
    if not re.fullmatch(r"[0-9a-f]{40,64}",base_sha):
        return {"base_status":"invalid","base_stale":True,"base_behind_by":None}
    if base_sha==main_sha:
        return {"base_status":"current","base_stale":False,"base_behind_by":0}
    if base_sha not in cache:
        exists=git(repo,"rev-parse","--verify",f"{base_sha}^{{commit}}",check=False)
        if exists.returncode:
            cache[base_sha]=("unavailable",None)
        elif ancestor(repo,base_sha,main_sha):
            behind=int(git(repo,"rev-list","--count",f"{base_sha}..{main_sha}").stdout.strip())
            cache[base_sha]=("behind",behind)
        else:
            cache[base_sha]=("diverged",None)
    status,behind=cache[base_sha]
    return {"base_status":status,"base_stale":True,"base_behind_by":behind}

def analyse(repo, prs, upstream_main, fork_dev):
    main_sha=git(repo,"rev-parse",upstream_main).stdout.strip()
    dev_tree=git(repo,"rev-parse",f"{fork_dev}^{{tree}}").stdout.strip()
    base_cache={}
    rows=[]
    for pr in sorted(prs,key=lambda x:int(x.get("number",0))):
        n=int(pr["number"]); head=pr.get("headRefOid") or ""; base_ref=pr.get("baseRefName") or ""
        row={"number":n,"title":pr.get("title",""),"url":pr.get("url",""),
             "author":(pr.get("author") or {}).get("login",""),"draft":bool(pr.get("isDraft")),
             "base_ref":base_ref,"base_sha":pr.get("baseRefOid"),"head_sha":head,"status":None,
             "changed_files":[],"patch_id":None,"merge_tree_main":None,"merge_tree_dev":None,
             "conflicts_main":[],"conflicts_dev":[]}
        if row["draft"]:
            row["base_status"]="draft"; row["base_stale"]=None; row["status"]="draft"; rows.append(row); continue
        if base_ref!="main":
            row["base_status"]="alternate"; row["base_stale"]=None; row["status"]="alternate base"; rows.append(row); continue
        row.update(base_state(repo,row["base_sha"],main_sha,base_cache))
        if not re.fullmatch(r"[0-9a-f]{40,64}",head):
            row["status"]="invalid head SHA"; rows.append(row); continue
        ref=f"refs/remotes/upstream/pr/{n}"
        got=git(repo,"rev-parse","--verify",f"{ref}^{{commit}}",check=False)
        if got.returncode:
            row["status"]="head unavailable"; rows.append(row); continue
        if got.stdout.strip()!=head:
            row["status"]="head changed during scan"; row["fetched_head_sha"]=got.stdout.strip(); rows.append(row); continue
        in_main=ancestor(repo,head,main_sha)
        in_dev=ancestor(repo,head,fork_dev)
        if in_main and in_dev:
            row["status"]="already in upstream main and dev"; rows.append(row); continue
        if in_main:
            row["status"]="already in upstream main"; rows.append(row); continue
        if in_dev:
            row["status"]="already in dev"; rows.append(row); continue
        try:
            row["changed_files"]=sorted(set(x for x in git(repo,"diff","--name-only",f"{upstream_main}...{head}").stdout.splitlines() if x))
            row["patch_id"]=patch_id(repo,upstream_main,head)
            m=merge_tree(repo,upstream_main,head); d=merge_tree(repo,fork_dev,head)
        except Exception as exc:
            row["status"]="analysis error"; row["analysis_error"]=str(exc)[-1000:]; rows.append(row); continue
        row.update(merge_tree_main=m.get("tree"),merge_tree_dev=d.get("tree"),
                   conflicts_main=m.get("conflicts",[]),conflicts_dev=d.get("conflicts",[]),
                   merge_error_main=m.get("error"),merge_error_dev=d.get("error"))
        if d["status"]=="clean" and d.get("tree")==dev_tree: row["status"]="already present in dev"
        elif d["status"]=="conflict": row["status"]="conflict with dev"
        elif d["status"]=="error": row["status"]="merge-tree error"
        elif m["status"]=="conflict": row["status"]="clean on dev; conflict with upstream main"
        elif m["status"]=="error": row["status"]="upstream merge-tree error"
        else: row["status"]="clean on dev"
        rows.append(row)
    groups=defaultdict(list)
    excluded={"head changed during scan","head unavailable","invalid head SHA","alternate base","draft",
              "analysis error","already in upstream main","already in upstream main and dev","already in dev","already present in dev"}
    for r in rows:
        if r.get("patch_id") and r.get("merge_tree_main") and r["status"] not in excluded:
            groups[(r["patch_id"],r["merge_tree_main"])].append(r["number"])
    duplicates=[{"prs":ns,"patch_id":k[0],"merge_tree":k[1]} for k,ns in sorted(groups.items()) if len(ns)>1]
    exact={pair for g in duplicates for pair in itertools.combinations(g["prs"],2)}
    hints=[h for h in overlap_hints(rows) if tuple(h["prs"]) not in exact]
    counts=defaultdict(int); base_counts=defaultdict(int)
    for r in rows:
        counts[r["status"]]+=1
        base_counts[r.get("base_status","unknown")]+=1
    stale=sum(1 for r in rows if r.get("base_stale") is True)
    return {"generated_utc":dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
            "upstream_main_sha":main_sha,"fork_dev_sha":fork_dev,"total_open_prs":len(rows),
            "counts":dict(sorted(counts.items())),"base_status_counts":dict(sorted(base_counts.items())),
            "stale_base_count":stale,"exact_duplicate_groups":duplicates,
            "overlap_hints":hints,"pull_requests":rows}

def markdown(report,max_pairs=30):
    lines=["# Upstream PR coordination triage","",f"Generated: {report['generated_utc']}",
           f"Upstream main: `{report['upstream_main_sha']}`",f"Fork dev: `{report['fork_dev_sha']}`",
           f"Open upstream PRs scanned: **{report['total_open_prs']}**",
           f"PRs with stale recorded base SHA: **{report['stale_base_count']}**","","## Status counts","","| Status | PRs |","|---|---:|"]
    lines += [f"| {k} | {v} |" for k,v in report["counts"].items()]
    lines += ["","## Base SHA status","","| Base status | PRs |","|---|---:|"]
    lines += [f"| {k} | {v} |" for k,v in report["base_status_counts"].items()]
    stale=[r for r in report["pull_requests"] if r.get("base_stale") is True]
    stale.sort(key=lambda r:(r.get("base_behind_by") if r.get("base_behind_by") is not None else -1,r["number"]),reverse=True)
    lines += ["","## Stale PR bases (still analyzed against current upstream main)",""]
    if stale:
        for r in stale[:max_pairs]:
            link=f"[#{r['number']}]({r['url']})" if r.get("url") else f"#{r['number']}"
            detail=f"{r['base_behind_by']} commits behind" if r.get("base_behind_by") is not None else r.get("base_status","unknown")
            lines.append(f"- {link}: recorded base is {detail}.")
        if len(stale)>max_pairs: lines.append(f"- … and {len(stale)-max_pairs} more; see `triage.json`.")
    else:
        lines.append("None detected.")
    lines += ["","## Exact duplicate candidates",""]
    if report["exact_duplicate_groups"]:
        for g in report["exact_duplicate_groups"][:max_pairs]:
            links=", ".join(f"[#{n}](https://github.com/Marvisak/VIctory/pull/{n})" for n in g["prs"])
            lines.append(f"- {links} — same stable patch-id and merge tree (`{g['patch_id'][:12]}`).")
    else:
        lines.append("None detected.")
    lines += ["","## Likely overlaps (review hints only)",""]
    if report["overlap_hints"]:
        for h in report["overlap_hints"][:max_pairs]:
            a,b=h["prs"]; links=f"[#{a}](https://github.com/Marvisak/VIctory/pull/{a}) / [#{b}](https://github.com/Marvisak/VIctory/pull/{b})"
            ev=h["reason"]+(("; shared issues "+", ".join(f"#{n}" for n in h["issues"])) if h["issues"] else "")
            ev+="; files "+", ".join("`"+p+"`" for p in h["files"][:5])
            lines.append(f"- {links}: {ev}.")
    else:
        lines.append("None detected.")
    lines += ["","## PRs requiring attention","","| PR | Status | Base | Files |","|---|---|---|---|"]
    clean={"clean on dev","already in dev","already present in dev","already in upstream main",
           "already in upstream main and dev","draft"}
    attention=[r for r in report["pull_requests"] if r["status"] not in clean or r.get("base_stale") is True]
    for r in attention[:100]:
        link=f"[#{r['number']}]({r['url']})" if r.get("url") else f"#{r['number']}"
        files=r.get("conflicts_dev",[])[:3] or r.get("changed_files",[])[:3]
        base=f"behind {r['base_behind_by']}" if r.get("base_behind_by") is not None else r.get("base_status","unknown")
        file_cell=", ".join("`"+p+"`" for p in files)
        lines.append(f"| {link} | {r['status']} | {base} | {file_cell} |")
    if not attention:
        lines.append("| — | No blockers detected | current | — |")
    lines += ["","Read-only report: no upstream code was executed; no remote PR, issue, label, branch, or ref was changed. Merge-tree/patch-id results are evidence, not merge approval.",""]
    return "\n".join(lines)

def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo",type=Path,required=True); ap.add_argument("--prs",type=Path,required=True)
    ap.add_argument("--upstream-main",default="refs/remotes/upstream/main")
    ap.add_argument("--fork-dev",default="HEAD"); ap.add_argument("--out",type=Path,required=True)
    a=ap.parse_args(); prs=json.loads(a.prs.read_text(encoding="utf-8"))
    dev=git(a.repo,"rev-parse",a.fork_dev).stdout.strip()
    report=analyse(a.repo,prs,a.upstream_main,dev); a.out.mkdir(parents=True,exist_ok=True)
    (a.out/"triage.json").write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    md=markdown(report); (a.out/"triage.md").write_text(md,encoding="utf-8")
    summary=os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary,"a",encoding="utf-8") as f: f.write(md)
    print(md)

if __name__=="__main__": main()
