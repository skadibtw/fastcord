#!/usr/bin/env python3
"""Synthetic local Text Display Markdown; no real-account or remote assets."""
import json
import sys
from pathlib import Path
world=json.loads((Path(__file__).resolve().parent.parent/'world.json').read_text())
author={'id':'99001','username':'fixture_app','global_name':'Markdown Fixture','bot':True}
world['users'].append(author)
components=[
 {'type':10,'content':'# Component Markdown\n**Bold** *italic* __underline__ ~~strike~~ and `code`\n> Quote with a [safe example](https://example.test)\n||FIRST SECRET <:fox:123>||'},
 {'type':17,'accent_color':0x5865F2,'components':[{'type':10,'content':'## A second Text Display\n||SECOND SECRET|| stays independent.\nUnicode: 🦊  Custom emoji fallback: <:fox:123>\n```rust\nlet local = true;\n```'}]},
 {'type':17,'spoiler':True,'components':[{'type':10,'content':'HIDDEN CONTAINER SECRET'}]},
]
world['messages']['6005']=[{'id':'1549999999999999999','channel_id':'6005','guild_id':'5001','type':0,'author':author,'content':'','flags':32768,'timestamp':'2026-11-01T12:00:00+00:00','components':components}]
Path(sys.argv[1]).write_text(json.dumps(world,ensure_ascii=False,indent=2)+'\n')
