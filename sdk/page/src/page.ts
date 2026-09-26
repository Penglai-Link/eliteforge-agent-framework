import type {Initialization,Response,Topic,ContextUpdate,PageContext} from "./protocol.js";
export * from "./protocol.js";
export class CapabilityPage {
  private pending=new Map<string,{resolve:(value:unknown)=>void;reject:(error:Error)=>void;timer:ReturnType<typeof setTimeout>}>();
  private topics=new Map<string,Set<(data:unknown)=>void>>();
  private contexts=new Set<(context:PageContext)=>void>();
  private closed=false;
  private constructor(private port:MessagePort,public readonly initialization:Initialization){
    port.onmessage=({data}:{data:Response|Topic|ContextUpdate|{kind:"close";session:string}})=>{
      if(this.closed||data.session!==initialization.session)return;
      if(data.kind==="close"){this.close();return;}
      if(data.kind==="context"){this.initialization.context=data.context;for(const callback of this.contexts)callback(data.context);return;}
      if(data.kind==="topic"){for(const callback of this.topics.get(data.topic)??[])callback(data.data);return;}
      const pending=this.pending.get(data.id);if(!pending)return;this.pending.delete(data.id);clearTimeout(pending.timer);
      data.error!==undefined?pending.reject(new Error(data.error)):pending.resolve(data.result);
    };port.onmessageerror=()=>this.close();port.start();window.addEventListener("pagehide",this.onPageHide);
  }
  static connect(timeoutMs=15000):Promise<CapabilityPage>{
    const params=new URLSearchParams(location.hash.slice(1));const session=params.get("capability-session");const nonce=params.get("capability-nonce");const origin=params.get("host-origin");
    if(!session||!nonce||!origin)return Promise.reject(new Error("Missing page binding"));
    return new Promise((resolve,reject)=>{
      const timer=setTimeout(()=>{window.removeEventListener("message",receive);reject(new Error("Page initialization timed out"));},timeoutMs);
      const receive=(event:MessageEvent)=>{
        if(event.source!==parent||event.origin!==origin||event.data?.kind!=="capability.initialize"||event.data.nonce!==nonce||event.data.initialization?.session!==session||event.ports.length!==1)return;
        clearTimeout(timer);window.removeEventListener("message",receive);resolve(new CapabilityPage(event.ports[0],event.data.initialization));
      };
      window.addEventListener("message",receive);parent.postMessage({kind:"capability.ready",session,nonce},origin);
    });
  }
  call<T=unknown>(method:string,params:unknown=null,timeoutMs=60000):Promise<T>{
    if(this.closed)return Promise.reject(new Error("Page closed"));
    if(this.pending.size>=128)return Promise.reject(new Error("Page request capacity exceeded"));
    const id=crypto.randomUUID();
    return new Promise<T>((resolve,reject)=>{
      const timer=setTimeout(()=>{this.pending.delete(id);this.port.postMessage({kind:"cancel",session:this.initialization.session,id});reject(new Error("Page call timed out; result may be unknown"));},timeoutMs);
      this.pending.set(id,{resolve:value=>resolve(value as T),reject,timer});
      this.port.postMessage({kind:"request",session:this.initialization.session,id,method,params});
    });
  }
  subscribe(topic:string,callback:(data:unknown)=>void):()=>void{
    if(this.closed||!this.initialization.topics.includes(topic))throw new Error("Topic not declared or page closed");
    const callbacks=this.topics.get(topic)??new Set();callbacks.add(callback);this.topics.set(topic,callbacks);
    this.port.postMessage({kind:"subscribe",session:this.initialization.session,topic});
    return()=>{callbacks.delete(callback);if(!callbacks.size){this.topics.delete(topic);if(!this.closed)this.port.postMessage({kind:"unsubscribe",session:this.initialization.session,topic});}};
  }
  onContext(callback:(context:PageContext)=>void):()=>void{this.contexts.add(callback);callback(this.initialization.context);return()=>{this.contexts.delete(callback);};}
  close():void{
    if(this.closed)return;this.closed=true;this.port.postMessage({kind:"close",session:this.initialization.session});this.port.close();
    window.removeEventListener("pagehide",this.onPageHide);
    for(const pending of this.pending.values()){clearTimeout(pending.timer);pending.reject(new Error("Page closed"));}
    this.pending.clear();this.topics.clear();this.contexts.clear();
  }
  private onPageHide=()=>this.close();
}
