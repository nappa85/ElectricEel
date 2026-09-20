#!/usr/bin/env python3
"""Merge app/translations/harbour-electric-eel.ts (lupdate template) into per-language files.

For each app/translations/harbour-electric-eel_<lang>.ts present: keep existing finished
translations whose source still exists, carry them onto matching new
messages (by source text, across contexts), leave genuinely new strings
unfinished, drop vanished ones (and the empty/stock-qsTrId entries that
must never ship). Run via tools/build-qm.sh, never by hand.
"""
import glob
import copy
import os
import sys
import xml.etree.ElementTree as ET

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TPL = os.path.join(REPO, "app", "translations", "harbour-electric-eel.ts")


def messages(path):
    """(context, source) -> (translation_text or None)."""
    out = {}
    for ctx in ET.parse(path).getroot().iter("context"):
        name = ctx.findtext("name") or ""
        for m in ctx.iter("message"):
            src = m.findtext("source") or ""
            if not src.strip():
                continue
            tr = m.find("translation")
            text = tr.text if tr is not None else None
            if tr is not None and tr.get("type") == "unfinished":
                text = None
            if text:
                out.setdefault(src, text)
    return out


def main():
    template_msgs = []  # (context, source, [location elements]) in order
    seen = set()
    for ctx in ET.parse(TPL).getroot().iter("context"):
        name = ctx.findtext("name") or ""
        for m in ctx.iter("message"):
            src = m.findtext("source") or ""
            if not src.strip() or (name, src) in seen:
                continue
            seen.add((name, src))
            template_msgs.append((name, src, list(m.findall("location"))))

    langs = sorted(
        os.path.basename(p)[len("harbour-electric-eel_") : -len(".ts")]
        for p in glob.glob(os.path.join(REPO, "app", "translations", "harbour-electric-eel_*.ts"))
    )
    if not langs:
        print("no language files (app/translations/harbour-electric-eel_<lang>.ts); template only")
        return

    for lang in langs:
        path = os.path.join(REPO, "app", "translations", f"harbour-electric-eel_{lang}.ts")
        old = messages(path)
        root = ET.Element("TS", {"version": "2.1", "language": lang})
        counts = {"kept": 0, "new": 0}
        by_ctx = {}
        for ctx_name, src, locations in template_msgs:
            by_ctx.setdefault(ctx_name, []).append((src, locations))
        for ctx_name, sources in by_ctx.items():
            ctx = ET.SubElement(root, "context")
            ET.SubElement(ctx, "name").text = ctx_name
            for src, locations in sources:
                m = ET.SubElement(ctx, "message")
                for loc in locations:
                    m.append(copy.deepcopy(loc))
                ET.SubElement(m, "source").text = src
                tr = ET.SubElement(m, "translation")
                if src in old:
                    tr.text = old[src]
                    counts["kept"] += 1
                else:
                    tr.set("type", "unfinished")
                    counts["new"] += 1
        ET.indent(root)
        ET.ElementTree(root).write(path, encoding="utf-8", xml_declaration=True)
        print(f"{lang}: kept={counts['kept']} new-unfinished={counts['new']}")


if __name__ == "__main__":
    sys.exit(main())
