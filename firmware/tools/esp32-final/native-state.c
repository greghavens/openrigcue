/* A second, separately written implementation of the CAN ISR/TX and
 * short handshake object writes, for comparison with the Rust code. The device
 * addresses are in can.rs and gate.rs. Only supplied frame bytes are used. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
static uint64_t hash=UINT64_C(14695981039346656037);
static void word(uint32_t v){for(unsigned i=0;i<4;i++){hash^=(v>>(8*i))&255;hash*=UINT64_C(1099511628211);}}
static void bytes(const uint8_t*p,unsigned n){word(n);for(unsigned i=0;i<n;i++)word(p[i]);}
static uint32_t seed;
static uint32_t rng(void){seed=seed*1664525+1013904223;return seed;}
static unsigned random_char(void){uint32_t a=rng();uint64_t m=(uint64_t)a*86;uint32_t lo=m;if(lo<86){uint32_t threshold=(-86u)%86;while(lo<threshold){m=(uint64_t)rng()*86;lo=m;}}unsigned out=40+(m>>32);return out==94?93:out;}
static uint32_t regs[64];
static uint32_t read_reg(unsigned off){word(1);word(off);word(regs[off/4]);return regs[off/4];}
static void write_reg(unsigned off,uint32_t v){word(2);word(off);word(v);regs[off/4]=v;}
static void store(unsigned off,uint32_t v,unsigned width,uint8_t*memory){word(3);word(off);word(v);word(width);for(unsigned i=0;i<width;i++)memory[off+i]=(v>>(i*8))&255;}
static void receive(unsigned board,unsigned ir,unsigned queue,unsigned ff,unsigned dlc,unsigned pattern){
 uint8_t memory[64], stale[8],data[8];for(unsigned i=0;i<64;i++)memory[i]=(i*29+pattern*71)^0xb6;
 unsigned frame=board?12:8,base=frame+8;uint32_t guard=0x91e2c3b4;memcpy(memory+28,&guard,4);memcpy(stale,memory+base,8);memcpy(data,stale,8);
 for(unsigned i=0;i<64;i++)regs[i]=0x12340000u+i*0x1937+pattern*0x4381;
 regs[3]=ir;regs[16]=(ff?128:0)|dlc;unsigned give=0,release=0,have=0;uint32_t fir=0,id=0;
 uint32_t actual_ir=read_reg(12);
 if(actual_ir&1){release=1;if(queue){fir=read_reg(64);store(frame,fir,4,memory);if(!(fir&128)){id=read_reg(68)<<3;id|=read_reg(72)>>5;}else{id=read_reg(68)<<21;id|=read_reg(72)<<13;id|=read_reg(76)<<5;id|=read_reg(80)>>3;}store(frame+4,id,4,memory);for(unsigned i=0;i<(fir&15);i++){uint8_t value=read_reg((fir&128?84:76)+4*i);store(base+i,value,1,memory);if(i<8)data[i]=value;}have=1;}}
 if(actual_ir&254){give=1;}
 uint32_t final_guard;memcpy(&final_guard,memory+28,4);word(have);word(give);word(release);word(final_guard!=guard);if(have){word(fir);word(id);bytes(data,8);}bytes(memory,64);
}
static void transmit(unsigned ff,unsigned dlc,unsigned pattern){
 for(unsigned i=0;i<64;i++)regs[i]=0xabc00000+i*19+pattern;
 uint32_t fir=(ff?128:0)|dlc,id=0x932a51bc+pattern;write_reg(64,fir);if(!ff){write_reg(68,id>>3);write_reg(72,id<<5);}else{write_reg(68,id>>21);write_reg(72,id>>13);write_reg(76,id>>5);write_reg(80,id<<3);}for(unsigned i=0;i<dlc;i++)write_reg((ff?84:76)+4*i,(((i*31+pattern*73)^0xa9u)&255));uint32_t v=read_reg(4);write_reg(4,v|1);
}
int main(void){
 for(unsigned board=0;board<2;board++)for(unsigned ir_i=0;ir_i<6;ir_i++)for(unsigned queue=0;queue<2;queue++)for(unsigned ff=0;ff<2;ff++)for(unsigned dlc=0;dlc<16;dlc++)for(unsigned pattern=0;pattern<2;pattern++){unsigned irs[]={0,1,2,3,254,255};receive(board,irs[ir_i],queue,ff,dlc,pattern);}
 for(unsigned board=0;board<2;board++)for(unsigned ff=0;ff<2;ff++)for(unsigned dlc=0;dlc<16;dlc++)for(unsigned pattern=0;pattern<2;pattern++)transmit(ff,dlc,pattern);
 for(unsigned board=0;board<2;board++)for(unsigned length=0;length<20;length++)for(unsigned status_i=0;status_i<5;status_i++)for(unsigned pattern=0;pattern<3;pattern++){
 uint8_t input[20],tail[16],output[16];for(unsigned i=0;i<20;i++)input[i]=pattern==1?'?':(pattern==2&&i==5?0:'a'+i);for(unsigned i=0;i<16;i++)tail[i]=i*17+length;memcpy(output,tail,16);unsigned n=0;while(n<length&&input[n]&&n<14)n++;uint8_t work[17]={0};memcpy(work,input,n);unsigned status[]={0,1,2,3,255};if(status[status_i]<3){work[n++]='^';work[n++]='0'+status[status_i];}seed=0x1234abcd;for(unsigned i=0;i<n;i++)output[i]=work[i]=='?'?random_char():work[i];bytes(output,16);
 }
 for(unsigned p=0;p<256;p++){uint8_t args[9];for(unsigned i=0;i<9;i++)args[i]=i*37+p;uint32_t old=0xdead0000|p;word((old&~0xcfu)|8);word(args[0]);bytes(args+1,8);uint8_t data[]={2,(uint8_t)(p+0x40),(uint8_t)(p^0x73),0xa9,0x4d,0xe2,0,0};word(((old&0xf0)|8)&0xffffff38u);word(0x7e8);bytes(data,8);}
 printf("native_state=%016llx\n",(unsigned long long)hash);
}
