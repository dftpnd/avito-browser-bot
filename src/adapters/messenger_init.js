(()=>{
 const chunk = Object.keys(window).find(k=>k.startsWith('messenger_mfe_') && Array.isArray(window[k]));
 if(!chunk) throw Error('Messenger runtime not loaded');
 window[chunk].push([['local_avito_export_'+Date.now()],{},r=>window.__avitoAssistantRequire=r]);
 const req=window.__avitoAssistantRequire;
 const stores=[];
 const seen=new Set();
 const check=o=>{if(o && typeof o.getState==='function' && typeof o.dispatch==='function' && !stores.includes(o)) stores.push(o)};
 const inspect=o=>{if(!o || typeof o!=='object')return;check(o);check(o.store);check(o.value);check(o.value?.store)};
 for(const el of document.querySelectorAll('input,main,div')) {
  const key=Object.keys(el).find(k=>k.startsWith('__reactFiber$'));
  if(!key)continue;
  let f=el[key];
  while(f&&!seen.has(f)) {seen.add(f);inspect(f.memoizedProps);inspect(f.memoizedState); f=f.return;}
 }
 let store=stores.find(s=>{const v=s.getState();return v.channels&&v.options}) || stores.find(s=>s.getState().channels);
 if(!store) return {ok:false,reason:'Redux store missing',stores:stores.map(s=>Object.keys(s.getState()))};
 const state=store.getState();
 const ctx=req(85).ZT(state);
 const client=req(107).KU(ctx);
 if(!client) return {ok:false,reason:'Read client missing',getter:req(107).KU.toString()};
 window.__avitoAssistant={client,store};
 return {ok:true,stateKeys:Object.keys(state),methods:Object.getOwnPropertyNames(Object.getPrototypeOf(client)),featureFlags:state.options?.messengerApico};
})()