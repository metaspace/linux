#!/usr/bin/env python3

import subprocess
import argparse

parser = argparse.ArgumentParser()
parser.add_argument('path')

args = parser.parse_args()

syms = [
    "__addsf3",
    "__eqsf2",
    "__gesf2",
    "__lesf2",
    "__ltsf2",
    "__mulsf3",
    "__nesf2",
    "__unordsf2",
    "__adddf3",
    "__ledf2",
    "__ltdf2",
    "__muldf3",
    "__unorddf2",
    "__muloti4",
    "__multi3",
    "__udivmodti4",
    "__udivti3",
    "__umodti3",
]

objcopy_args = [f'--redefine-sym {x}=__rust{x}' for x in syms]

cmd = ['objcopy'] + objcopy_args + [args.path]

subprocess.run(' '.join(cmd), shell=True, check=True)

