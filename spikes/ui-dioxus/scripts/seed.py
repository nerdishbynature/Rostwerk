#!/usr/bin/env python3
"""Creates a tenant with masterdata and N partners (default 10000) through the API.

Prints the tenant id and a dev token for the UI. Needs the spike API (scripts/api.sh)
and ROSTWERK_ADMIN_TOKEN from .env.
"""
import json
import random
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

API = "http://127.0.0.1:18080"
N = int(sys.argv[1]) if len(sys.argv) > 1 else 10000

env = Path(__file__).resolve().parents[3] / ".env"
admin = next(l.split("=", 1)[1].strip() for l in env.read_text().splitlines()
             if l.startswith("ROSTWERK_ADMIN_TOKEN="))


def call(method, path, token, body=None):
    req = urllib.request.Request(API + path, method=method,
                                 data=json.dumps(body).encode() if body else None,
                                 headers={"Authorization": f"Bearer {token}",
                                          "Content-Type": "application/json"})
    with urllib.request.urlopen(req) as res:
        return json.load(res)


tenant = call("POST", "/admin/tenants", admin,
              {"name": "UI Spike GmbH", "modules": ["masterdata", "accounting"]})["id"]
token = f"dev:{tenant}:spike"

rng = random.Random(34)
first = ["Müller", "Schmidt", "Schneider", "Fischer", "Weber", "Meyer", "Wagner", "Becker",
         "Schulz", "Hoffmann", "Koch", "Richter", "Klein", "Wolf", "Schröder", "Neumann"]
trade = ["Bau", "Metall", "Holz", "Elektro", "Logistik", "Software", "Druck", "Feinkost",
         "Maschinen", "Textil", "Glas", "Kunststoff", "Agrar", "Medizintechnik"]
form = ["GmbH", "AG", "KG", "GmbH & Co. KG", "e.K.", "UG (haftungsbeschränkt)", "OHG"]
kinds = ["customer"] * 6 + ["supplier"] * 3 + ["both"]


def partner(i):
    name = f"{rng.choice(first)} {rng.choice(trade)} {rng.choice(form)}"
    slug = name.split()[0].lower().replace("ü", "ue").replace("ö", "oe")
    email = f"info{i}@{slug}-{rng.choice(trade).lower()}.de" if rng.random() < 0.8 else None
    return {"name": name, "kind": rng.choice(kinds), "email": email}


bodies = [partner(i) for i in range(N)]
with ThreadPoolExecutor(16) as pool:
    for done, _ in enumerate(pool.map(lambda b: call("POST", "/v1/partners", token, b), bodies), 1):
        if done % 1000 == 0:
            print(f"{done} partners", file=sys.stderr)

print(json.dumps({"tenant": tenant, "token": token}))
