(async()=>{
    const i=__INTENT__;const a=window.__avitoAssistant;
    if(!a)throw Error('Messenger session expired');
    const raw=await a.client.request('messenger.history.v2',{channelId:i.chat_id,limit:100},{cache:false});
    if(raw.error||!Array.isArray(raw.result?.items))throw Error('Fresh history unavailable');
    const items=raw.result.items;
    const stable=m=>JSON.stringify({id:String(m.id),fromUid:m.fromUid??null,body:m.body??null,type:m.type??null,created:m.created??null,isDeleted:m.isDeleted??null});
    const expected=new Map(i.expected_head.map(([id,value])=>[id,JSON.parse(value)]));
    const canon=v=>Array.isArray(v)?v.map(canon):v&&typeof v==='object'?Object.fromEntries(Object.keys(v).sort().map(k=>[k,canon(v[k])])):v;
    const normalize=o=>JSON.stringify(canon(o));
    const changed=items.some(m=>{const old=expected.get(String(m.id));return !old||normalize(JSON.parse(stable(m)))!==normalize(old)})||[...expected.keys()].some(id=>!items.some(m=>String(m.id)===id));
    if(changed)return {state:'changed',messages:items};
    const result=await window.__avitoAssistantRequire(270).W(a.store.getState(),{channelId:i.chat_id,text:i.text,randomId:i.random_id});
    if(result?.error||!result?.result?.id)throw Error('Send result unknown');
    return {state:'sent',message:result.result};
  })()
