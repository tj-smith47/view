#!/usr/bin/env python3
"""Independent production-line counter, written against the same rules as
scripts/audit-god-files.sh but with a different algorithm, to satisfy the
port spec's property (4): cross-check the counter before trusting it.

Differences that make it a real second opinion rather than a transcription:
  - character state machine returns an explicit token stream state per line
    instead of awk's string-rebuilding elision
  - test regions are found by scanning forward for a balanced brace span or a
    statement terminator using an explicit depth stack, not a per-line
    accumulator carried in globals
  - reads the whole file at once and indexes lines, rather than a streaming
    FNR==1 reset
"""
import re
import sys

CFG = re.compile(r"^\s*#\[cfg\(")
TEST_TOKEN = re.compile(r"(^|[(,\s])test([),]|$)")
NEG = re.compile(r"not\s*\(")
ANY = re.compile(r"\(any\(")


def is_test_only_cfg(line: str) -> bool:
    if not CFG.search(line):
        return False
    if ANY.search(line):
        return False
    if NEG.search(line):
        return False
    return bool(TEST_TOKEN.search(line))


def strip_lines(text: str):
    """Yield (code_only, ) per line with strings/chars/comments removed."""
    out, buf = [], []
    i, n = 0, len(text)
    in_block = in_str = False
    in_raw = False
    raw_hashes = 0
    while i < n:
        c = text[i]
        # a line boundary is emitted in EVERY state: a raw string, block
        # comment or backslash-continued string spans lines without ending
        # them, and swallowing the newline merges real code lines into one
        if c == "\n":
            out.append("".join(buf))
            buf = []
            i += 1
            continue
        if in_raw:
            end = '"' + "#" * raw_hashes
            if text.startswith(end, i):
                in_raw = False
                i += len(end)
            else:
                i += 1
            continue
        if in_str:
            if c == "\\":
                # never step over a newline: `"... \` continues the string on
                # the next line, and that next line is still a code line
                i += 1 if (i + 1 < n and text[i + 1] == "\n") else 2
            elif c == '"':
                in_str = False
                i += 1
            else:
                i += 1
            continue
        if in_block:
            if text.startswith("*/", i):
                in_block = False
                i += 2
            else:
                i += 1
            continue
        if text.startswith("//", i):
            j = text.find("\n", i)
            i = j if j != -1 else n
            continue
        if text.startswith("/*", i):
            in_block = True
            i += 2
            continue
        if c == "r":
            m = 0
            while i + 1 + m < n and text[i + 1 + m] == "#":
                m += 1
            if i + 1 + m < n and text[i + 1 + m] == '"':
                in_raw, raw_hashes = True, m
                i += m + 2
                continue
        if c == '"':
            in_str = True
            i += 1
            continue
        if c == "'":
            if text.startswith("\\", i + 1):
                j = text.find("'", i + 2)
                if j != -1:
                    i = j + 1
                    continue
            elif i + 2 < n and text[i + 2] == "'":
                i += 3
                continue
        buf.append(c)
        i += 1
    out.append("".join(buf))
    return out


def count(path: str) -> int:
    text = open(path, encoding="utf-8", errors="replace").read()
    code = strip_lines(text)
    raw = text.split("\n")
    n = len(code)
    prod = 0
    i = 0
    while i < n:
        # a region opens only on an attribute that survives elision
        if is_test_only_cfg(code[i]):
            depth = 0
            opened = False
            while i < n:
                depth += code[i].count("{") - code[i].count("}")
                if "{" in code[i]:
                    opened = True
                if opened:
                    if depth <= 0:
                        break
                elif code[i].rstrip().endswith(";"):
                    break
                i += 1
            i += 1
            continue
        if code[i].strip():
            prod += 1
        i += 1
    return prod


if __name__ == "__main__":
    for p in sys.argv[1:]:
        print(f"{count(p)}\t{p}")
