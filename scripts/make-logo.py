#!/usr/bin/env python3
"""Draws a logo: the SwapMarket icon and the site's name as outlines, so it looks the same in every browser.
Needs fontTools (pip install fonttools) and Noto Sans Bold Italic. Run from the repo root.
Usage: scripts/make-logo.py [liq2sol|sol2base|eth2base] [OUT]   (default liq2sol, into client/site/logo.svg; the sister sites
keep theirs as logo.svg in their own repositories)

The icon is the one from SwapMarket/swapmarket.github.io (public/sm_icon.svg), kept here at scripts/sm_icon.svg.
"""
import re, sys
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.transformPen import TransformPen
FONT='/usr/share/fonts/truetype/noto/NotoSans-BoldItalic.ttf'
ICON=open('scripts/sm_icon.svg').read()
f=TTFont(FONT); gs=f.getGlyphSet(); cmap=f.getBestCmap(); upm=f['head'].unitsPerEm
# The icon, in its own 500-unit box: ink spans x 3..483, y 19..494, and the long bottom edge is at y 441.5.
IX0, IX1, IY0, IY1, EDGE = 3, 483, 19, 494, 441.5
# Each chain's name in its colour: Liquid's teal, Solana's gradient, Base's blue, Ethereum's blue-violet. The 2 is white.
SOLANA=('#9945ff', '#14f195')
LOGOS={
    'liq2sol': [('liq', '#46b3a3'), ('2', '#ffffff'), ('sol', SOLANA)],
    'sol2base': [('sol', SOLANA), ('2', '#ffffff'), ('base', '#3d82ff')],
    'eth2base': [('eth', '#8a9cf0'), ('2', '#ffffff'), ('base', '#3d82ff')],
}
name=sys.argv[1] if len(sys.argv) > 1 else 'liq2sol'
out=sys.argv[2] if len(sys.argv) > 2 else 'client/site/logo.svg'
words=LOGOS[name]
text=''.join(w for w, _ in words)
colours=[f'url(#{w})' if isinstance(c, tuple) else c for w, c in words for _ in w]
def glyph(ch, x=0):
    # Placed and flipped in the path itself, so a gradient spans the letters in the image's own coordinates.
    name=cmap[ord(ch)]; pen=SVGPathPen(gs); gs[name].draw(TransformPen(pen, (1, 0, 0, -1, x, 0))); bp=BoundsPen(gs); gs[name].draw(bp)
    return pen.getCommands(), gs[name].width, bp.bounds
# Letter heights, in font units: the tallest letter sets the icon's size. Taken from liq2sol for every logo, so all three have the
# same height and letter size, and sit alike in the same menu.
ascent=max(glyph(c)[2][3] for c in 'liq2sol' if glyph(c)[2]); descent=min(glyph(c)[2][1] for c in 'liq2sol' if glyph(c)[2])
scale=1.12*ascent/(EDGE-IY0)          # icon units to font units: top of icon a little above the letters
icon_w=(IX1-IX0)*scale
gap=0.32*upm
x=icon_w+gap
paths=[]; starts=[]
for ch,col in zip(text,colours):
    d,adv,b=glyph(ch, x)
    starts.append(x)
    paths.append(f'<path fill="{col}" d="{d}"/>')
    x+=adv
# A gradient spans its word's letters.
grads=[]; i=0
for w, c in words:
    if isinstance(c, tuple):
        x1=starts[i+len(w)] if i+len(w) < len(starts) else x
        grads.append(f'<linearGradient id="{w}" gradientUnits="userSpaceOnUse" x1="{starts[i]:.1f}" y1="0" x2="{x1:.1f}" y2="0"><stop offset="0" stop-color="{c[0]}"/><stop offset="1" stop-color="{c[1]}"/></linearGradient>')
    i+=len(w)
top=min(-ascent, -(EDGE-IY0)*scale)                  # y grows down; baseline at 0
bottom=max(-descent, (IY1-EDGE)*scale)
pad=0.04*upm
inner=re.search(r'<g transform=.*</g>', ICON, re.S).group(0)
icon=f'<g transform="translate({-IX0*scale:.2f} {-EDGE*scale:.2f}) scale({scale:.5f})">{inner}</g>'
vb=f'{-pad:.1f} {top-pad:.1f} {x+2*pad:.1f} {bottom-top+2*pad:.1f}'
svg=f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}" role="img" aria-label="{name}">
<!-- The SwapMarket icon (SwapMarket/swapmarket.github.io, public/sm_icon.svg) and {name} in Noto Sans Bold Italic (SIL OFL 1.1), as outlines.
     The icon's long bottom edge sits on the letters' baseline. -->
<defs>{"".join(grads)}</defs>
{icon}
{"".join(paths)}
</svg>
'''
open(out,'w').write(svg)
print('viewBox', vb, '| aspect', round((x+2*pad)/(bottom-top+2*pad),3), '| em height share', round(upm/(bottom-top+2*pad),3))
