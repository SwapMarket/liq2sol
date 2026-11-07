#!/usr/bin/env python3
"""Draws client/site/logo.svg: the SwapMarket icon and liq2sol as outlines, so it looks the same in every browser.
Needs fontTools (pip install fonttools) and Noto Sans Bold Italic. Run from the repo root.

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
text='liq2sol'
colours=['#46b3a3']*3+['#f7931a']+['url(#sol)']*3
def glyph(ch, x=0):
    # Placed and flipped in the path itself, so a gradient spans the letters in the image's own coordinates.
    name=cmap[ord(ch)]; pen=SVGPathPen(gs); gs[name].draw(TransformPen(pen, (1, 0, 0, -1, x, 0))); bp=BoundsPen(gs); gs[name].draw(bp)
    return pen.getCommands(), gs[name].width, bp.bounds
# Letter heights, in font units: the tallest letter sets the icon's size.
ascent=max(glyph(c)[2][3] for c in text if glyph(c)[2]); descent=min(glyph(c)[2][1] for c in text if glyph(c)[2])
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
sol_x0, sol_x1 = starts[4], x
top=min(-ascent, -(EDGE-IY0)*scale)                  # y grows down; baseline at 0
bottom=max(-descent, (IY1-EDGE)*scale)
pad=0.04*upm
inner=re.search(r'<g transform=.*</g>', ICON, re.S).group(0)
icon=f'<g transform="translate({-IX0*scale:.2f} {-EDGE*scale:.2f}) scale({scale:.5f})">{inner}</g>'
vb=f'{-pad:.1f} {top-pad:.1f} {x+2*pad:.1f} {bottom-top+2*pad:.1f}'
svg=f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}" role="img" aria-label="liq2sol">
<!-- The SwapMarket icon (SwapMarket/swapmarket.github.io, public/sm_icon.svg) and liq2sol in Noto Sans Bold Italic (SIL OFL 1.1), as outlines.
     The icon's long bottom edge sits on the letters' baseline. -->
<defs><linearGradient id="sol" gradientUnits="userSpaceOnUse" x1="{sol_x0:.1f}" y1="0" x2="{sol_x1:.1f}" y2="0"><stop offset="0" stop-color="#9945ff"/><stop offset="1" stop-color="#14f195"/></linearGradient></defs>
{icon}
{"".join(paths)}
</svg>
'''
open('client/site/logo.svg','w').write(svg)
print('viewBox', vb, '| aspect', round((x+2*pad)/(bottom-top+2*pad),3), '| em height share', round(upm/(bottom-top+2*pad),3))
