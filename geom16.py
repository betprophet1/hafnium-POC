import io,re,html
s=io.open('prediction-exchange.html',encoding='utf-8').read(); body=s[s.index('</style>'):]
def txt(h): return re.sub(r'\s+',' ',html.unescape(re.sub(r'<[^>]*>',' ',h))).strip()
SIZE={'lbl-hd':12,'lbl-sub':10.5,'lbl-edge':10}
def attrs(a): return dict(re.findall(r'([\w:-]+)="([^"]*)"',a))
figs=[(m.start(),m.group(0)) for m in re.finditer(r'<figure>.*?</figure>',s,re.S)]
def fig_of(sec,k):
    out=[f for f in figs if re.findall(r'<section id="s(\d+)">',s[:f[0]])[-1]==str(sec)]; return out[k-1][1]
for sec,k in ((18,1),(18,2),(18,3)):
    svg=re.sub(r'<defs>.*?</defs>','',re.search(r'<svg.*?</svg>',fig_of(sec,k),re.S).group(0),flags=re.S)
    vb=list(map(float,re.search(r'viewBox="([^"]+)"',svg).group(1).split())); texts=[]; segs=[]
    for m in re.finditer(r'<text\b([^>]*)>(.*?)</text>',svg,re.S):
        at=attrs(m.group(1)); cls=(at.get('class','').split() or [''])[0]; size=float(at.get('font-size',SIZE.get(cls,10.5)))
        t=html.unescape(re.sub(r'<[^>]*>','',m.group(2)))
        if 'x' not in at or 'y' not in at or t.strip().isdigit(): continue
        x,y=float(at['x']),float(at['y']); w=len(t)*0.6*size; x0={'start':x,'middle':x-w/2,'end':x-w}[at.get('text-anchor','start')]
        texts.append((t,(x0,y-size*0.78,x0+w,y+size*0.22)))
    for m in re.finditer(r'<rect\b([^>]*)/?>',svg):
        at=attrs(m.group(1))
        if all(q in at for q in ('x','y','width','height')):
            x,y,w,h=(float(at[q]) for q in ('x','y','width','height')); segs+=[((x,y),(x+w,y)),((x+w,y),(x+w,y+h)),((x,y+h),(x+w,y+h)),((x,y),(x,y+h))]
    for m in re.finditer(r'<path\b([^>]*)/?>',svg):
        pts=[tuple(map(float,q)) for q in re.findall(r'[ML]\s*([-\d.]+)[ ,]+([-\d.]+)',attrs(m.group(1)).get('d',''))]; segs+=list(zip(pts,pts[1:]))
    for m in re.finditer(r'<line\b([^>]*)/?>',svg):
        at=attrs(m.group(1))
        if all(q in at for q in ('x1','y1','x2','y2')): segs.append(((float(at['x1']),float(at['y1'])),(float(at['x2']),float(at['y2']))))
    def hit(bb,seg):
        (x1,y1),(x2,y2)=seg; X0,Y0,X1,Y1=bb
        if x1==x2: return X0<x1<X1 and max(min(y1,y2),Y0)<min(max(y1,y2),Y1)
        if y1==y2: return Y0<y1<Y1 and max(min(x1,x2),X0)<min(max(x1,x2),X1)
        return any(X0<x1+(x2-x1)*i/40<X1 and Y0<y1+(y2-y1)*i/40<Y1 for i in range(41))
    iss=[]
    for i,(t,bb) in enumerate(texts):
        if bb[0]<vb[0] or bb[2]>vb[0]+vb[2]: iss.append('outside %r'%t)
        iss+=['%r crosses %s'%(t,g) for g in segs if hit(bb,g)]
        iss+=['%r overlaps %r'%(t,t2) for t2,b2 in texts[i+1:] if bb[0]<b2[2] and b2[0]<bb[2] and bb[1]<b2[3] and b2[1]<bb[3]]
    print('s%d fig %d geometry: %d labels, issues %d %s'%(sec,k,len(texts),len(iss),iss[:4]))
