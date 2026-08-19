import re, os
base = r'C:\Users\Mr-Music\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\windows-0.62.2'
wam = open(base + r'\src\Windows\Win32\UI\WindowsAndMessaging\mod.rs', encoding='utf-8', errors='replace').read()
for name in ['WM_SETFONT','WM_DPICHANGED','WM_GETMINMAXINFO','WM_CTLCOLOREDIT','WM_CTLCOLORSTATIC','WM_COMMAND','WM_NOTIFY','WM_DRAWITEM']:
    i = wam.find('WM_SETFONT')
    # find all lines containing the name
    for m in re.finditer(rf'^.*{name}.*$', wam, re.M):
        print(name, '=>', m.group(0)[:150])
        break
    else:
        print(name, '=> NOT FOUND as line')
# Also check HFONT/HGDIOBJ conversions
gdi = open(base + r'\src\Windows\Win32\Graphics\Gdi\mod.rs', encoding='utf-8', errors='replace').read()
for m in re.finditer(r'pub struct HGDIOBJ[^}]*\}', gdi):
    print('HGDIOBJ:', m.group(0)[:200])
for m in re.finditer(r'impl From<HFONT> for HGDIOBJ[^}]*\}', gdi):
    print('From HFONT:', m.group(0)[:200])
# SendMessageW signature
i = wam.find('pub unsafe fn SendMessageW')
print(wam[i:i+700])
