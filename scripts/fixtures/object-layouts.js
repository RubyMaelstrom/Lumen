// Diagnostic workloads, not browser/product scores. All results escape into a checked checksum.
var CASE = '__CASE__';
var ring = new Array(1024);
function Record(x) {
    this.id=x; this.a=x+1; this.b=x+2; this.c=x+3;
    this.d=x+4; this.e=x+5; this.f=x+6; this.next=null;
}
function literal(x) { return {id:x,a:x+1,b:x+2,c:x+3,d:x+4,e:x+5,f:x+6,next:null}; }
function construct(x) { return new Record(x); }
function dynamic(x) {
    var o={}; o['k'+(x&3)]=x; o.value=x+1; o.next=null; return o;
}
function batch(n, factory) {
    var sum=0;
    for(var i=0;i<n;i++) {
        var o=factory(i); ring[i&1023]=o;
        sum=(sum+(o.a===undefined?o.value:o.a))|0;
    }
    for(var i=0;i<1024;i++)sum=(sum+(ring[i].id||ring[i].value))|0;
    return sum;
}
var factory=CASE==='literal'?literal:CASE==='construct'?construct:dynamic;
if(CASE==='json-state') {
    var initial=[];
    for(var i=0;i<128;i++)initial.push(literal(i));
    var input=JSON.stringify(initial);
    function jsonBatch(n) {
        var sum=0;
        for(var i=0;i<n;i++) {
            var state=JSON.parse(input);
            for(var j=0;j<state.length;j++){state[j].a+=i;state[j].next={flag:j&1};}
            sum=(sum+JSON.stringify(state).length+state[17].a)|0;
        }
        return sum;
    }
    jsonBatch(5);
    var start=Date.now();var checksum=jsonBatch(500);
} else {
    batch(10000,factory);
    var start=Date.now();var checksum=batch(1000000,factory);
}
console.log(JSON.stringify({case:CASE,elapsed_ms:Date.now()-start,checksum:checksum}));
