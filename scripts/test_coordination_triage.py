import tempfile
import unittest
from pathlib import Path
import coordination_triage as t

class Repo(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory(prefix="victory-triage-"); self.repo=Path(self.tmp.name)/"r"
        self.repo.mkdir(); t.git(self.repo,"init","--initial-branch=main")
        t.git(self.repo,"config","user.name","Triage"); t.git(self.repo,"config","user.email","triage@example.invalid")
        (self.repo/"base.txt").write_text("base\n"); t.git(self.repo,"add","."); t.git(self.repo,"commit","-m","base")
        self.base=t.git(self.repo,"rev-parse","HEAD").stdout.strip()
    def tearDown(self): self.tmp.cleanup()
    def commit(self,branch,path,text,msg):
        t.git(self.repo,"switch","-C",branch,self.base); (self.repo/path).write_text(text)
        t.git(self.repo,"add","."); t.git(self.repo,"commit","-m",msg)
        return t.git(self.repo,"rev-parse","HEAD").stdout.strip()
    def test_merge_tree_clean_and_no_checkout(self):
        head=self.commit("topic","new.txt","change\n","topic"); before=t.git(self.repo,"rev-parse","HEAD").stdout.strip()
        r=t.merge_tree(self.repo,self.base,head)
        self.assertEqual(r["status"],"clean"); self.assertTrue(r["tree"])
        self.assertEqual(t.git(self.repo,"rev-parse","HEAD").stdout.strip(),before)
    def test_merge_tree_conflict(self):
        head=self.commit("topic","base.txt","topic\n","topic"); t.git(self.repo,"switch","-C","main",self.base)
        (self.repo/"base.txt").write_text("main\n"); t.git(self.repo,"commit","-am","main")
        main=t.git(self.repo,"rev-parse","HEAD").stdout.strip(); r=t.merge_tree(self.repo,main,head)
        self.assertEqual(r["status"],"conflict"); self.assertIn("base.txt",r["conflicts"])
    def test_exact_duplicate_requires_patch_and_tree(self):
        a=self.commit("a","new.txt","same\n","a"); b=self.commit("b","new.txt","same\n","b")
        self.assertEqual(t.patch_id(self.repo,self.base,a),t.patch_id(self.repo,self.base,b))
        self.assertEqual(t.merge_tree(self.repo,self.base,a)["tree"],t.merge_tree(self.repo,self.base,b)["tree"])
    def test_analysis_pins_base_head_and_fork_dev(self):
        head=self.commit("topic","new.txt","change\n","topic")
        t.git(self.repo,"update-ref","refs/remotes/upstream/main",self.base)
        t.git(self.repo,"update-ref","refs/remotes/upstream/pr/1",head)
        p={"number":1,"title":"Add file","headRefOid":head,"baseRefName":"main","baseRefOid":self.base,
           "isDraft":False,"closingIssuesReferences":[]}
        r=t.analyse(self.repo,[p],"refs/remotes/upstream/main",self.base)
        self.assertEqual(r["pull_requests"][0]["status"],"clean on dev")
        self.assertEqual(r["pull_requests"][0]["base_status"],"current")
        self.assertFalse(r["pull_requests"][0]["base_stale"])
        p["baseRefOid"]="f"*40
        r=t.analyse(self.repo,[p],"refs/remotes/upstream/main",self.base)
        self.assertEqual(r["pull_requests"][0]["base_status"],"unavailable")
        self.assertTrue(r["pull_requests"][0]["base_stale"])
        self.assertEqual(r["pull_requests"][0]["status"],"clean on dev")
    def test_stale_base_is_flagged_but_still_analyzed(self):
        head=self.commit("topic","topic.txt","topic\\n","topic")
        t.git(self.repo,"switch","-C","main",self.base)
        (self.repo/"main.txt").write_text("main\\n"); t.git(self.repo,"add","."); t.git(self.repo,"commit","-m","advance main")
        main=t.git(self.repo,"rev-parse","HEAD").stdout.strip()
        t.git(self.repo,"update-ref","refs/remotes/upstream/main",main)
        t.git(self.repo,"update-ref","refs/remotes/upstream/pr/1",head)
        p={"number":1,"title":"Add topic file","headRefOid":head,"baseRefName":"main","baseRefOid":self.base,
           "isDraft":False,"closingIssuesReferences":[]}
        r=t.analyse(self.repo,[p],"refs/remotes/upstream/main",self.base)
        row=r["pull_requests"][0]
        self.assertEqual(row["base_status"],"behind")
        self.assertTrue(row["base_stale"])
        self.assertEqual(row["base_behind_by"],1)
        self.assertEqual(row["status"],"clean on dev")
        self.assertIn("topic.txt",row["changed_files"])
        self.assertEqual(r["stale_base_count"],1)

    def test_head_already_in_upstream_main_is_classified(self):
        head=self.commit("topic","topic.txt","topic\\n","topic")
        t.git(self.repo,"switch","-C","main",head)
        t.git(self.repo,"update-ref","refs/remotes/upstream/main",head)
        t.git(self.repo,"update-ref","refs/remotes/upstream/pr/1",head)
        p={"number":1,"title":"Merged topic","headRefOid":head,"baseRefName":"main","baseRefOid":head,
           "isDraft":False,"closingIssuesReferences":[]}
        r=t.analyse(self.repo,[p],"refs/remotes/upstream/main",self.base)
        self.assertEqual(r["pull_requests"][0]["status"],"already in upstream main")

    def test_exact_duplicate_groups_are_reported_and_removed_from_overlap_hints(self):
        a=self.commit("a","new.txt","same\\n","a")
        b=self.commit("b","new.txt","same\\n","b")
        t.git(self.repo,"update-ref","refs/remotes/upstream/main",self.base)
        t.git(self.repo,"update-ref","refs/remotes/upstream/pr/1",a)
        t.git(self.repo,"update-ref","refs/remotes/upstream/pr/2",b)
        prs=[{"number":n,"title":"Add identical file","headRefOid":sha,"baseRefName":"main","baseRefOid":self.base,
              "isDraft":False,"closingIssuesReferences":[]} for n,sha in [(1,a),(2,b)]]
        r=t.analyse(self.repo,prs,"refs/remotes/upstream/main",self.base)
        self.assertEqual(r["exact_duplicate_groups"][0]["prs"],[1,2])
        self.assertEqual(r["overlap_hints"],[])

    def test_overlaps_are_hints_only_and_need_shared_files(self):
        items=[{"number":1,"title":"Fix descriptor heap bounds","changed_files":["heap.rs"],"closingIssuesReferences":[{"number":7}]},
               {"number":2,"title":"Fix descriptor heap bounds","changed_files":["heap.rs"],"closingIssuesReferences":[{"number":7}]},
               {"number":3,"title":"Fix loader parsing","changed_files":["loader.rs"],"closingIssuesReferences":[]}]
        self.assertEqual([x["prs"] for x in t.overlap_hints(items)],[[1,2]])

if __name__=="__main__": unittest.main()
