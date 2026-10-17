"""Verify the scoped manifest loader hooks without launching QQ."""

import argparse
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class LoaderTest(unittest.TestCase):
    loader_path = None

    def test_manifest_reads_and_original_write_paths(self):
        with tempfile.TemporaryDirectory(prefix="qqtui loader '") as temp:
            work = Path(temp)
            compiler = shlex.split(os.environ.get("CC") or "cc")
            cflags = shlex.split(os.environ.get("CFLAGS", ""))
            ldflags = shlex.split(os.environ.get("LDFLAGS", ""))
            loader = self.loader_path or work / "loader.so"
            if self.loader_path is None:
                subprocess.run(
                    compiler
                    + cflags
                    + ["-shared", "-fPIC", str(ROOT / "bridge/loader.c")]
                    + ldflags
                    + ["-o", str(loader), "-ldl"],
                    check=True,
                )
            original, replacement, unrelated = [
                work / name for name in ["original", "replacement", "unrelated"]
            ]
            original.write_text("original")
            replacement.write_text("replacement")
            unrelated.write_text("unrelated")
            source = work / "probe.c"
            source.write_text(r"""#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
int main(int argc,char **argv){
  char b[32]={0}; int f=open(argv[1],O_RDONLY);read(f,b,31);close(f);printf("%s\n",b);
  FILE *s=fopen(argv[2],"rb");memset(b,0,32);fread(b,1,31,s);fclose(s);printf("%s\n",b);
  f=open(argv[1],O_WRONLY|O_TRUNC);write(f,"written",7);close(f);return 0;
}""")
            probe = work / "probe"
            subprocess.run(
                compiler + cflags + [str(source)] + ldflags + ["-o", str(probe)],
                check=True,
            )
            handle = loader.open("rb")
            self.addCleanup(handle.close)
            env = {
                **os.environ,
                "LD_PRELOAD": f"/proc/{os.getpid()}/fd/{handle.fileno()}",
                "QQTUI_PACKAGE": str(original),
                "QQTUI_MANIFEST": str(replacement),
            }
            result = subprocess.check_output(
                [probe, original, unrelated], env=env, text=True
            )
            self.assertEqual(result, "replacement\nunrelated\n")
            self.assertEqual(original.read_text(), "written")
            self.assertEqual(replacement.read_text(), "replacement")



if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--loader", type=Path)
    args, remaining = parser.parse_known_args()
    LoaderTest.loader_path = args.loader.resolve() if args.loader else None
    unittest.main(argv=[sys.argv[0], *remaining])
