"""Verify the scoped manifest loader hooks without launching QQ."""

import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class LoaderTest(unittest.TestCase):
    loader_path = None

    def test_manifest_reads_and_original_write_paths(self):
        with tempfile.TemporaryDirectory(prefix="qqtui-loader-") as temp:
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
            data = work / "path with ' quotes"
            data.mkdir()
            copied_loader = data / "loader.so"
            shutil.copy2(loader, copied_loader)
            loader = copied_loader
            original, replacement, unrelated = [
                data / name for name in ["original", "replacement", "unrelated"]
            ]
            original.write_text("original")
            replacement.write_text("replacement")
            unrelated.write_text("unrelated")
            source = work / "probe.c"
            source.write_text(r"""#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <stdlib.h>
#include <dlfcn.h>
#include <sys/socket.h>
#include <errno.h>
static char owned[4096], sibling[4096];
int NodeContextifyContextMetrics1(char **out,int capacity,int width){
  const char *names[]={"/official.js",owned,"/external.js",sibling};
  if(out&&width>0)for(int i=0;i<4&&i<capacity;i++){
    size_t length=strlen(names[i]),size=length<(size_t)width?length:(size_t)width-1;
    memcpy(out[i],names[i]+length-size,size);out[i][size]=0;
  }
  return 4;
}
int main(int argc,char **argv){
  char b[32]={0}; int f=open(argv[1],O_RDONLY);read(f,b,31);close(f);printf("%s\n",b);
  FILE *s=fopen(argv[2],"rb");memset(b,0,32);fread(b,1,31,s);fclose(s);printf("%s\n",b);
  unsetenv("QQTUI_PACKAGE");unsetenv("QQTUI_MANIFEST");
  s=fopen(argv[1],"rb");memset(b,0,32);fread(b,1,31,s);fclose(s);printf("%s\n",b);
  snprintf(owned,sizeof(owned),"%s/bridge.cjs",argv[3]);
  snprintf(sibling,sizeof(sibling),"%s-other/bridge.cjs",argv[3]);
  int (*metrics)(char **,int,int)=dlsym(RTLD_DEFAULT,"NodeContextifyContextMetrics1");
  char slots[3][8];char *out[]={slots[0],slots[1],slots[2]};
  int total=metrics(NULL,0,0),copied=metrics(out,3,8);
  printf("metrics:%d:%d:%d:%d:%d\n",total,copied,!strcmp(slots[0],"cial.js"),!strcmp(slots[1],"rnal.js"),!strcmp(slots[2],sibling+strlen(sibling)-7));
  printf("bounds:%d:%d:%d\n",metrics(out,1,8),metrics(out,0,8),metrics(out,3,0));
  int (*next)(void)=dlsym(RTLD_DEFAULT,"next_marker");printf("next:%d\n",next());
  void *bugly=dlopen(argv[4],RTLD_NOW);int (*connect_reporter)(void)=dlsym(bugly,"bugly_connect");
  int denied=connect_reporter();int normal=connect(-1,0,0);
  printf("bugly:%d:%d\n",denied,normal==-1&&errno==EBADF);dlclose(bugly);
  f=open(argv[1],O_WRONLY|O_TRUNC);write(f,"written",7);close(f);return 0;
}""")
            probe = work / "probe"
            subprocess.run(
                compiler + cflags + [str(source), "-Wl,--export-dynamic", "-ldl"] + ldflags + ["-o", str(probe)],
                check=True,
            )
            handle = loader.open("rb")
            self.addCleanup(handle.close)
            handles = [handle]
            for name, code in [
                ("before", '#include <dlfcn.h>\nint marker(void){return 10;}\nint next_marker(void){int (*next)(void)=dlsym(RTLD_NEXT,"marker");return next?next():-1;}'),
                ("after", 'int marker(void){return 20;}'),
                ("libbugly", '#include <sys/socket.h>\n#include <errno.h>\nint bugly_connect(void){return connect(-1,0,0)==-1&&errno==EACCES;}'),
            ]:
                library_source = work / f"{name}.c"
                library_source.write_text(code)
                library = data / f"{name}.so"
                # Match the PLT calls in QQ's shipped native reporter.
                subprocess.run(compiler + cflags + ["-fplt", "-shared", "-fPIC", str(library_source), "-ldl"] + ldflags + ["-o", str(library)], check=True)
                if name != "libbugly":
                    opened = library.open("rb")
                    self.addCleanup(opened.close)
                    handles.append(opened)
            env = {
                **os.environ,
                "LD_PRELOAD": ":".join(f"/proc/{os.getpid()}/fd/{file.fileno()}" for file in handles),
                "LD_AUDIT": f"/proc/{os.getpid()}/fd/{handle.fileno()}",
                "QQTUI_PACKAGE": str(original),
                "QQTUI_MANIFEST": str(replacement),
            }
            result = subprocess.check_output(
                [probe, original, unrelated, data, data / "libbugly.so"], env=env, text=True
            )
            self.assertEqual(result, "replacement\nunrelated\noriginal\nmetrics:3:3:1:1:1\nbounds:1:-1:-1\nnext:20\nbugly:1:1\n")
            self.assertEqual(original.read_text(), "written")
            self.assertEqual(replacement.read_text(), "replacement")



if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--loader", type=Path)
    args, remaining = parser.parse_known_args()
    LoaderTest.loader_path = args.loader.resolve() if args.loader else None
    unittest.main(argv=[sys.argv[0], *remaining])
