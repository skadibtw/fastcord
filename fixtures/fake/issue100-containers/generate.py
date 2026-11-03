#!/usr/bin/env python3
"""Synthetic Components V2 containers; no live account data or outbound actions."""
import json
import sys
from pathlib import Path
world = json.loads((Path(__file__).resolve().parent.parent / 'world.json').read_text())
author = {'id':'99001','username':'fixture_app','global_name':'Container Fixture','bot':True}
world['users'].append(author)
def text(content):
    return {'type':10,'content':content}
button = {'type':2,'style':1,'custom_id':'visible-action','label':'Visible action (do not submit)'}
components = [
    {'type':17,'spoiler':True,'components':[text('HIDDEN SPOILER SECRET MUST NOT APPEAR'), {'type':1,'components':[{'type':2,'style':1,'custom_id':'hidden','label':'HIDDEN ACTION MUST NOT APPEAR'}]}]},
    {'type':17,'accent_color':0x5865F2,'components':[text('Blue accent. Ordinary controls remain available.'),{'type':1,'components':[button]}]},
    {'type':17,'accent_color':0,'components':[text('Black accent')]},
    {'type':17,'accent_color':0xFFFFFF,'components':[text('White accent')]},
    {'type':17,'accent_color':None,'components':[text('Neutral container without an accent')]},
    {'type':17,'spoiler':'false','components':[text('MALFORMED FLAG SECRET MUST NOT APPEAR')]},
]
world['messages']['6005']=[{'id':'1549999999999999999','channel_id':'6005','guild_id':'5001','type':0,'author':author,'content':'','flags':32768,'timestamp':'2026-10-15T12:00:00+00:00','components':components}]
Path(sys.argv[1]).write_text(json.dumps(world,ensure_ascii=False,indent=2)+'\n')
