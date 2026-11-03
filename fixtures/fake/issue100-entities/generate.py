#!/usr/bin/env python3
"""Synthetic local-only entity selector world; never uses live account data."""
import json
import sys
from pathlib import Path
root = Path(__file__).resolve().parent
world = json.loads((root.parent/'world.json').read_text())
app = {'id':'99001','username':'fixture_app','global_name':'Entity Fixture','bot':True}
world['users'].append(app)
controls = [
 {'type':5,'custom_id':'users','placeholder':'Choose loaded users','min_values':1,'max_values':2},
 {'type':6,'custom_id':'roles','placeholder':'Choose loaded roles','min_values':0,'max_values':2,'default_values':[{'id':'99999','type':'role'}]},
 {'type':7,'custom_id':'mentionable','placeholder':'Choose users or roles','min_values':0,'max_values':3},
 {'type':8,'custom_id':'channels','placeholder':'Choose accessible text channels','min_values':0,'max_values':2,'channel_types':[0]},
]
message={'id':'1549999999999999999','channel_id':'6005','guild_id':'5001','type':0,'author':app,
 'content':'Local entity selector fixture. Submit remains behind the existing verification gate.',
 'timestamp':'2026-10-15T12:00:00+00:00','components':[{'type':1,'components':[control]} for control in controls]}
world['messages']['6005']=[message]
Path(sys.argv[1]).write_text(json.dumps(world,ensure_ascii=False,indent=2)+'\n')
