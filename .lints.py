import re

s = open('/tmp/clippy3.log', 'rb').read().decode('utf-8', 'replace')
lines = s.splitlines()

def count(sub):
    return sum(1 for l in lines if sub in l)

lints = {
    'result-large-err': count('result-large-err'),
    'large-enum-variant': count('large-enum-variant'),
    'dead-code': count('-D dead-code'),
    'needless': count('needless'),
    'manual': count('manual-'),
    'type-complexity': count('type-complexity'),
    'io-other-error': count('io-other-error'),
    'field-reassign': count('field-reassign'),
    'derivable': count('derivable'),
    'wildcard': count('wildcard'),
    'too-many': count('too-many'),
    'collapsible': count('collapsible'),
    'redundant': count('redundant'),
    'manual-range': count('manual-range'),
    'large': count('large'),
    'suspicious': count('suspicious'),
    'unused': count('unused'),
    'mutable': count('mutable'),
    'cloned': count('cloned'),
}
for name, n in lints.items():
    if n:
        print(name, n)
