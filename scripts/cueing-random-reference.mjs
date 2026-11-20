// A separate BigInt implementation of MT19937 array seeding and of ranged
// random integers from little-endian bit draws with rejection sampling.
import fs from 'node:fs';
const uint = n => Number(BigInt.asUintN(32, BigInt(n)));
function generator(seed) {
  const mt = [19650218];
  for(let i=1;i<624;i++) mt[i]=uint(1812433253n*BigInt(mt[i-1]^(mt[i-1]>>>30))+BigInt(i));
  const key=[uint(seed)]; if(seed>>32n)key.push(uint(seed>>32n));
  let i=1,j=0;
  for(let k=Math.max(624,key.length);k;k--){
    mt[i]=uint(BigInt(mt[i]) ^ (BigInt(mt[i-1]^(mt[i-1]>>>30))*1664525n));
    mt[i]=uint(BigInt(mt[i])+BigInt(key[j])+BigInt(j));
    if(++i>=624){mt[0]=mt[623];i=1;} if(++j>=key.length)j=0;
  }
  for(let k=623;k;k--){
    mt[i]=uint((BigInt(mt[i]) ^ (BigInt(mt[i-1]^(mt[i-1]>>>30))*1566083941n))-BigInt(i));
    if(++i>=624){mt[0]=mt[623];i=1;}
  }
  mt[0]=0x80000000;let index=624;
  const word=()=>{
    if(index===624){
      for(let k=0;k<624;k++){
        const y=(mt[k]&0x80000000)|(mt[(k+1)%624]&0x7fffffff);
        mt[k]=uint(mt[(k+397)%624]^(y>>>1)^((y&1)?0x9908b0df:0));
      } index=0;
    }
    let y=mt[index++]; y^=y>>>11; y^=(y<<7)&0x9d2c5680; y^=(y<<15)&0xefc60000;y^=y>>>18;return y>>>0;
  };
  const bits=k=>{let result=0n;for(let s=0;s<k;s+=32)result|=BigInt(word()>>>(32-Math.min(32,k-s)))<<BigInt(s);return result;};
  return (lo,hi)=>{const n=hi-lo+1n,k=n.toString(2).length;let r;do{r=bits(k);}while(r>=n);return lo+r;};
}
const cases=[];
const ranges=[[1n,400n],[-(1n<<63n),(1n<<63n)-1n],[1n,1n<<64n],[3n,(1n<<100n)+17n],[-(1n<<120n),1n<<100n],[-(1n<<127n),(1n<<127n)-1n],[-(1n<<127n),-(1n<<127n)+1n],[(1n<<127n)-10n,(1n<<127n)-1n]];
for(const seed of [0n,1n,42n,12345n,(1n<<40n)+7n,(1n<<64n)-1n]){
  const randint=generator(seed);
  for(const [low,high] of ranges) for(let i=0;i<8;i++) cases.push([seed,low,high,randint(low,high)].join(' '));
}
fs.writeFileSync('crates/openrigcue-cueing/tests/data/random-wide-reference.txt',cases.join('\n')+'\n');
console.log(`${cases.length} reference draws generated`);
