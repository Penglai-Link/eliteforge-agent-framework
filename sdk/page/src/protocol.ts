export interface InstanceIdentity {package:string;version:string;digest:string;scope:string;configuration_version:string}
export interface PageContext {theme:string;language:string;data:unknown}
export interface Initialization {protocol:1;session:string;instance:InstanceIdentity;page:string;context:PageContext;permissions:string[];topics:string[]}
export type Request={kind:"request";id:string;session:string;method:string;params:unknown};
export type Response={kind:"response";id:string;session:string;result?:unknown;error?:string};
export type Topic={kind:"topic";session:string;topic:string;data:unknown};
export type ContextUpdate={kind:"context";session:string;context:PageContext};
