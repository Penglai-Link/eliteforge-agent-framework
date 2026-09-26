import type {Initialization,PageContext,Request} from "./protocol.js";
export interface PageBridge {
  initialization:Initialization;
  resourceUrl:string;
  call(method:string,params:unknown,signal:AbortSignal):Promise<unknown>;
  close():void|Promise<void>;
}
/** Host owns the container, routing and backend binding. No framework-specific UI dependencies. */
export function mountCapabilityPage(container:HTMLElement,bridge:PageBridge){
  const iframe=document.createElement("iframe");iframe.title=bridge.initialization.page;iframe.sandbox.add("allow-scripts");
  const nonce=crypto.randomUUID();const url=new URL(bridge.resourceUrl,location.href);url.hash=new URLSearchParams({"capability-session":bridge.initialization.session,"capability-nonce":nonce,"host-origin":location.origin}).toString();
  const channel=new MessageChannel();const subscriptions=new Set<string>();const active=new Map<string,AbortController>();let closed=false;let connected=false;
  const close=()=>{
    if(closed)return;closed=true;clearTimeout(timer);window.removeEventListener("message",ready);channel.port1.postMessage({kind:"close",session:bridge.initialization.session});
    for(const controller of active.values())controller.abort();active.clear();subscriptions.clear();channel.port1.close();iframe.remove();void bridge.close();
  };
  const ready=(event:MessageEvent)=>{
    if(closed||connected||event.source!==iframe.contentWindow||event.data?.kind!=="capability.ready"||event.data.session!==bridge.initialization.session||event.data.nonce!==nonce)return;
    connected=true;clearTimeout(timer);window.removeEventListener("message",ready);
    iframe.contentWindow!.postMessage({kind:"capability.initialize",nonce,initialization:bridge.initialization},"*",[channel.port2]);
  };
  const timer=setTimeout(close,15000);window.addEventListener("message",ready);
  channel.port1.onmessage=async({data}:{data:Request|{kind:"subscribe"|"unsubscribe"|"close"|"cancel";session:string;topic?:string;id?:string}})=>{
    if(closed||data.session!==bridge.initialization.session)return;
    if(data.kind==="close"){close();return;}
    if(data.kind==="cancel"){if(data.id)active.get(data.id)?.abort();return;}
    if(data.kind==="subscribe"||data.kind==="unsubscribe"){if(data.topic&&bridge.initialization.topics.includes(data.topic)){data.kind==="subscribe"?subscriptions.add(data.topic):subscriptions.delete(data.topic);}return;}
    if(data.kind!=="request"||active.has(data.id))return;
    if(active.size>=128){channel.port1.postMessage({kind:"response",session:data.session,id:data.id,error:"Page request capacity exceeded"});return;}
    const controller=new AbortController();active.set(data.id,controller);
    try{const result=await bridge.call(data.method,data.params,controller.signal);if(!closed&&!controller.signal.aborted)channel.port1.postMessage({kind:"response",session:data.session,id:data.id,result});}
    catch(error){if(!closed)channel.port1.postMessage({kind:"response",session:data.session,id:data.id,error:String(error)});}
    finally{active.delete(data.id);}
  };channel.port1.start();iframe.src=url.href;container.append(iframe);
  return {iframe,close,publish(topic:string,data:unknown){if(!closed&&subscriptions.has(topic))channel.port1.postMessage({kind:"topic",session:bridge.initialization.session,topic,data});},context(context:PageContext){if(!closed)channel.port1.postMessage({kind:"context",session:bridge.initialization.session,context});}};
}
