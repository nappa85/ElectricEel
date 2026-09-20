#!/usr/bin/env python3
"""Shared writer for fill-*.py batches: positional translation lists.

Each LANGS entry is an ordered list matching the lupdate template order
(156 messages). Validates list length and %N placeholder preservation,
then writes finished _<lang>.ts files. Run tools/build-qm.sh afterwards.
"""
import os
import re
import sys
import xml.etree.ElementTree as ET

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TPL = os.path.join(REPO, "app", "translations", "harbour-electric-eel.ts")


def template_sources():
    tree = ET.parse(TPL)
    return [(m.findtext("source") or "") for ctx in tree.getroot().iter("context")
            for m in ctx.iter("message")]


def placeholders(s):
    return sorted(set(re.findall(r"%\d", s)))


def write_all(langs):
    sources = template_sources()
    print(f"template: {len(sources)} messages")
    for lang, items in langs.items():
        assert len(items) == len(sources), \
            f"{lang}: {len(items)} items, template has {len(sources)}"
        tree = ET.parse(TPL)
        root = tree.getroot()
        root.set("language", lang)
        bad = 0
        for m, src, tr in zip(
                [m for ctx in root.iter("context") for m in ctx.iter("message")],
                sources, items):
            want = placeholders(src)
            got = placeholders(tr)
            if want != got:
                print(f"  PLACEHOLDER MISMATCH [{lang}] {src!r} -> {tr!r}")
                bad += 1
            te = m.find("translation")
            if te is None:
                te = ET.SubElement(m, "translation")
            te.text = tr
            if te.get("type") == "unfinished":
                del te.attrib["type"]
        out = os.path.join(REPO, "app", "translations",
                           f"harbour-electric-eel_{lang}.ts")
        ET.indent(root)
        tree.write(out, encoding="utf-8", xml_declaration=True)
        print(f"{lang}: wrote {len(items)}, placeholder issues: {bad}")
