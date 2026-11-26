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

def analyse(repo, prs, upstream_main, fork_dev):
    main_sha=git(repo,"rev-parse",upstream_main).stdout.strip()
    dev_tree=git(repo,"rev-parse",f"{fork_dev}^{{tree}}").stdout.strip()
    rows=[]
    for pr in sorted(prs,key=lambda x:int(x.get("number",0))):
        n=int(pr["number"]); head=pr.get("headRefOid") or ""; base_ref=pr.get("baseRefName") or ""
        row={"number":n,"title":pr.get("title",""),"url":pr.get("url",""),
             "author":(pr.get("author") or {}).get("login",""),"draft":bool(pr.get("isDraft")),
             "base_ref":base_ref,"base_sha":pr.get("baseRefOid"),"head_sha":head,"status":None,
             "changed_files":[],"patch_id":None,"merge_tree_main":None,"merge_tree_dev":None,
             "conflicts_main":[],"conflicts_dev":[]}
        if row["draft"]: row["status"]="draft"; rows.append(row); continue
        if base_ref!="main": row["status"]="alternate base"; rows.append(row); continue
        if not re.fullmatch(r"[0-9a-f]{40,64}",head): row["status"]="invalid head SHA"; rows.append(row); continue
        if row["base_sha"] and row["base_sha"]!=main_sha:
            row["status"]="base changed during scan"; row["observed_upstream_main_sha"]=main_sha; rows.append(row); continue
        ref=f"refs/remotes/upstream/pr/{n}"
        got=git(repo,"rev-parse","--verify",f"{ref}^{{commit}}",check=False)
        if got.returncode: row["status"]="head unavailable"; rows.append(row); continue
        if got.stdout.strip()!=head:
            row["status"]="head changed during scan"; row["fetched_head_sha"]=got.stdout.strip(); rows.append(row); continue
        if ancestor(repo,head,fork_dev): row["status"]="already in dev"; rows.append(row); continue
        row["changed_files"]=sorted(set(x for x in git(repo,"diff","--name-only",f"{upstream_main}...{head}").stdout.splitlines() if x))
        row["patch_id"]=patch_id(repo,upstream_main,head)
        m=merge_tree(repo,upstream_main,head); d=merge_tree(repo,fork_dev,head)
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
    for r in rows:
        if r.get("patch_id") and r.get("merge_tree_main") and r["status"] not in {
            "head changed during scan","head unavailable","invalid head SHA","base changed during scan","alternate base","draft"}:
            groups[(r["patch_id"],r["merge_tree_main"])].append(r["number"])
    duplicates=[{"prs":ns,"patch_id":k[0],"merge_tree":k[1]} for k,ns in sorted(groups.items()) if len(ns)>1]
    exact={pair for g in duplicates for pair in itertools.combinations(g["prs"],2)}
    hints=[h for h in overlap_hints(rows) if tuple(h["prs"]) not in exact]
    counts=defaultdict(int)
    for r in rows: counts[r["status"]]+=1
    return {"generated_utc":dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
            "upstream_main_sha":main_sha,"fork_dev_sha":fork_dev,"total_open_prs":len(rows),
            "counts":dict(sorted(counts.items())),"exact_duplicate_groups":duplicates,
            "overlap_hints":hints,"pull_requests":rows}

def markdown(report,max_pairs=30):
    lines=["# Upstream PR coordination triage","",f"Generated: {report['generated_utc']}",
           f"Upstream main: `{report['upstream_main_sha']}`",f"Fork dev: `{report['fork_dev_sha']}`",
           f"Open upstream PRs scanned: **{report['total_open_prs']}**","","## Status counts","","| Status | PRs |","|---|---:|"]
    lines += [f"| {k} | {v} |" for k,v in report["counts"].items()]
    lines += ["","## Exact duplicate candidates",""]
    if report["exact_duplicate_groups"]:
        for g in report["exact_duplicate_groups"][:max_pairs]:
            links=", ".join(f"[#{n}](https://github.com/Marvisak/VIctory/pull/{n})" for n in g["prs"])
            lines.append(f"- {links} — same stable patch-id and merge tree (`{g['patch_id'][:12]}`).")
    else: lines.append("None detected.")
    lines += ["","## Likely overlaps (review hints only)",""]
    if report["overlap_hints"]:
        for h in report["overlap_hints"][:max_pairs]:
            a,b=h["prs"]; links=f"[#{a}](https://github.com/Marvisak/VIctory/pull/{a}) / [#{b}](https://github.com/Marvisak/VIctory/pull/{b})"
            ev=h["reason"]+(("; shared issues "+", ".join(f"#{n}" for n in h["issues"])) if h["issues"] else "")
            ev+="; files "+", ".join(f"`{p}`" for p in h["files"][:5])
            lines.append(f"- {links}: {ev}.")
    else: lines.append("None detected.")
    lines += ["","## PRs requiring attention","","| PR | Status | Files |","|---|---|---|"]
    attention=[r for r in report["pull_requests"] if r["status"] not in {"clean on dev","already in dev","already present in dev","draft"}]
    for r in attention[:100]:
        link=f"[#{r['number']}]({r['url']})" if r.get("url") else f"#{r['number']}"
        files=r.get("conflicts_dev",[])[:3] or r.get("changed_files",[])[:3]
        lines.append(f"| {link} | {r['status']} | {', '.join(f'`{p}`' for p in files)} |")
    if not attention: lines.append("| — | No blockers detected | — |")
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
