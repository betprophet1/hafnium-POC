import io,re,html,collections
s=io.open('prediction-exchange.html',encoding='utf-8').read()
body=s[s.index('</style>'):]
bad=0
for t in re.findall(r'<table>.*?</table>',s,re.S):
    hd=re.search(r'<thead>.*?</thead>',t,re.S)
    if not hd: continue
    n=len(re.findall(r'<th[ >]',hd.group(0)))
    for row in re.findall(r'<tr>.*?</tr>',t[hd.end():],re.S):
        c=len(re.findall(r'<td[ >]',row))
        if c and c!=n: bad+=1; print(' MISMATCH',n,c,re.sub(r'<[^>]*>',' ',row)[:90])
css=s[:s.index('</style>')]; defined=set(re.findall(r'\.([a-z][a-z0-9-]*)',css)); used=set()
for m in re.finditer(r'class="([^"]+)"',body): used|=set(m.group(1).split())
print('table col mismatches',bad,'| undefined classes',sorted(used-defined),'| curly',sum(s.count(c) for c in '‘’“”'),'| ldquo/rdquo',s.count('&ldquo;'),s.count('&rdquo;'),'| dangling refs',[x for x in re.findall(r'&sect;(\d+)',s) if int(x)>23])
print('non-ascii beyond the existing set:', {c for c in body if ord(c)>127} - {'—','→','−','×','≤','·'})
for w in ['shard','Shard','v2 &middot;','Five nodes','Four groups','previous draft','earlier draft','tier-3','target rate','per session per day','three or five','price 1..99','This was missing','Nine of the open','on throughput grounds','dedicated core slot']:
    c=body.count(w)
    if c: print('  LEFTOVER %r %d'%(w,c))
print('leftover scan done')
SIZE={'lbl-hd':12,'lbl-sub':10.5,'lbl-edge':10}
def attrs(a): return dict(re.findall(r'([\w:-]+)="([^"]*)"',a))
for sec in (3,9,10):
    a=s.index('<section id="s%d">'%sec); f=s.index('<figure>',a); svg=re.sub(r'<defs>.*?</defs>','',s[s.index('<svg',f):s.index('</svg>',f)],flags=re.S)
    vb=list(map(float,re.search(r'viewBox="([^"]+)"',svg).group(1).split())); texts=[]; segs=[]
    for m in re.finditer(r'<text\b([^>]*)>(.*?)</text>',svg,re.S):
        at=attrs(m.group(1)); cls=(at.get('class','').split() or [''])[0]; size=float(at.get('font-size',SIZE.get(cls,10.5)))
        t=html.unescape(re.sub(r'<[^>]*>','',m.group(2)))
        if 'x' not in at or 'y' not in at or t.strip().isdigit(): continue
        x,y=float(at['x']),float(at['y']); w=len(t)*0.6*size; x0={'start':x,'middle':x-w/2,'end':x-w}[at.get('text-anchor','start')]
        texts.append((t,(x0,y-size*0.78,x0+w,y+size*0.22)))
    for m in re.finditer(r'<rect\b([^>]*)/?>',svg):
        at=attrs(m.group(1))
        if all(k in at for k in ('x','y','width','height')):
            x,y,w,h=(float(at[k]) for k in ('x','y','width','height')); segs+=[((x,y),(x+w,y)),((x+w,y),(x+w,y+h)),((x,y+h),(x+w,y+h)),((x,y),(x,y+h))]
    for m in re.finditer(r'<path\b([^>]*)/?>',svg):
        pts=[tuple(map(float,q)) for q in re.findall(r'[ML]\s*([-\d.]+)[ ,]+([-\d.]+)',attrs(m.group(1)).get('d',''))]; segs+=list(zip(pts,pts[1:]))
    for m in re.finditer(r'<line\b([^>]*)/?>',svg):
        at=attrs(m.group(1))
        if all(k in at for k in ('x1','y1','x2','y2')): segs.append(((float(at['x1']),float(at['y1'])),(float(at['x2']),float(at['y2']))))
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
    print('s%d figure geometry: %d labels, issues %d %s'%(sec,len(texts),len(iss),iss[:4]))
