// Retained object graph diagnostic, not a timed product benchmark.
function record(x) { return {id:x,a:x+1,b:x+2,c:x+3,d:x+4,e:x+5,f:x+6,next:null}; }
var records=new Array(30000);
for(var n=0;n<records.length;n++)records[n]=record(n);
for(var n=1;n<records.length;n++)records[n].next=records[n-1];
console.log(JSON.stringify({checksum:records[29999].next.id+records[1234].f,count:records.length}));
