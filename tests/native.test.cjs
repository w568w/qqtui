const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {QqResources}=require('../bridge/qq-resources.cjs');
const { NativeBridge, nativePeer, message } = require('../bridge/native.cjs');

function fixture(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qqtui-native-test-'));

  const endpoint = process.platform === 'win32'
    ? "\\\\.\\pipe\\" + path.basename(dir)
    : path.join(dir, 'bridge.sock');
  const b = new NativeBridge(endpoint, {stateDir:dir});
  const journals=[b.journalStore];
  t.after(async()=>{for(const journal of journals)await journal.close();fs.rmSync(dir,{recursive:true,force:true});});
  b.account = '123'; b.phase = 'ready';
  b.presentation.speech=async()=>({state:'allowed',scope:'none',reason:'',until:null});
  b.session = { getMSFService: () => ({ getServerTime: () => '1700000000' }) };
  let count = 0;
  b.msg = {
    generateMsgUniqueId: () => '90071992547409931',
    sendMsg: async () => { count++; return { result: 0 }; },
  };
  return { b, count: () => count, dir,journals };
}
const peer = { chat_type: 1, uid: 'u_self' };
test('older quoted messages resolve by ID and by exact native sequence', async t => {
  const {b}=fixture(t);
  const original={msgId:'90071992547409933',msgSeq:'18446744073709551613',chatType:1,peerUid:'u_self',elements:[{textElement:{content:'older original'}}]};
  const source={msgId:'90071992547409934',chatType:1,peerUid:'u_self',elements:[{replyElement:{replayMsgId:original.msgId,replayMsgSeq:original.msgSeq}}]};
  let sequences=0;
  b.msg.getMsgsByMsgId=async(_peer,ids)=>({result:0,msgList:ids[0]===source.msgId?[source]:ids[0]===original.msgId?[original]:[]});
  b.msg.getSingleMsg=async(_peer,seq)=>{sequences++;assert.equal(seq,original.msgSeq);return{result:0,msgList:[original]};};
  const byID=await b.call('messages.quote',{account:'123',peer,id:source.msgId},{});
  assert.equal(byID.messages[0].text,'older original');assert.equal(sequences,0);
  source.elements[0].replyElement.replayMsgId='0';
  const normalized=message(source,'123');assert.equal(normalized.reply_to,null);assert.equal(normalized.reply_seq,original.msgSeq);
  const bySeq=await b.call('messages.quote',{account:'123',peer,id:source.msgId},{});
  assert.equal(bySeq.messages[0].id,original.msgId);assert.equal(sequences,1);
  assert.deepEqual((await b.call('messages.quote',{account:'123',peer,id:original.msgId},{})).messages,[]);
  await assert.rejects(b.call('messages.quote',{account:'other',peer,id:source.msgId},{}),/Account changed/);

});
test('QQ faces use local upstream assets and retain a text fallback', t => {
  const {dir} = fixture(t);
  fs.writeFileSync(path.join(dir, '14.png'), 'asset');
  const raw = {msgId:'1',chatType:1,peerUid:'u_self',elements:[{elementId:'2',faceElement:{faceIndex:14,faceText:'微笑'}}]};
  const face = message(raw, '123', new QqResources({emojiDir:dir}));
  assert.equal(face.text, '[微笑]');
  assert.equal(face.attachments[0].kind, 'emoji');
  assert.equal(face.attachments[0].path, path.join(dir, '14.png'));
  assert.equal(message(raw, '123').attachments.length, 0);
  raw.elements[0].faceElement.faceIndex = '../14';
  assert.equal(message(raw, '123', new QqResources({emojiDir:dir})).attachments.length, 0);
});
test('peer validation excludes unimplemented scene types before native calls', () => {
  assert.throws(() => nativePeer({ chat_type: 7, uid: '0' }));
  assert.throws(() => nativePeer({ chat_type: 1, uid: '' }));
  assert.equal(nativePeer(peer).peerUid, 'u_self');
});
test('native IDs remain exact; controls are rendered as inert text', () => {
  const m = message({ msgId: '90071992547409931', chatType: 1, peerUid: 'u_self', senderUin: '123', elements: [{ textElement: { content: '\x1b]52;c;foo\x07中文👨‍👩‍👧‍👦' } }] }, '123');
  assert.equal(m.id, '90071992547409931');
  assert.equal(m.is_self, true);
  assert.ok(!m.text.includes('\x1b'));
});
test('recalled native history is redacted without a prior live recall event', () => {
  const raw = { msgId: '7697460219897322940', msgSeq: '687', chatType: 1, peerUid: 'u_self', elements: [
    { grayTipElement: { subElementType: 1, revokeElement: { wording: '因为有错别字。\t' } } },
    { textElement: { content: 'old body' } },
    { fileElement: { fileName: 'old.png', filePath: '/private/old.png' } },
    { replyElement: { replayMsgId: '123' } },
  ] };
  const m = message(raw, '123');
  assert.equal(m.recalled, true);
  assert.equal(m.text, '对方撤回了一条消息：因为有错别字。\t');
  assert.deepEqual(m.attachments, []);
  assert.equal(m.reply_to, null);
  raw.elements = [{ grayTipElement: { subElementType: 17, jsonGrayTipElement: { recentAbstract: 'system notice' } } }];
  const notice = message(raw, '123');
  assert.equal(notice.recalled, false);
  assert.equal(notice.text, 'system notice');
});
test('concurrent duplicate submissions only call native send once and survive bridge restart', async t => {
  const { b, count, dir,journals } = fixture(t);
  const p = { account: '123', epoch:b.epoch, peer, request_id: 'dedup-test', text: 'hello' };
  const [a, c] = await Promise.all([b.send(p, nativePeer(peer)), b.send(p, nativePeer(peer))]);
  assert.equal(a.native_id, c.native_id); assert.equal(count(), 1);
  const restarted = new NativeBridge(path.join(dir, 'bridge.sock'));
  journals.push(restarted.journalStore);
  restarted.account = '123';
  assert.equal((await restarted.send(p, nativePeer(peer))).state, 'unknown');
  await assert.rejects(b.send({ ...p, text: 'different' }, nativePeer(peer)), /reused/);
});
test('RPC acceptance cannot downgrade an earlier native confirmation', async t => {
  const { b } = fixture(t);
  b.clients.add({ subscribed: true, destroyed: true });
  b.msg.sendMsg = async (_id, target) => {
    b.messages([{ msgId: '90071992547409933', guildId: target.guildId, peerUid: 'u_self', chatType: 1, senderUin: '123', sendStatus: 2, elements: [] }]);
    return { result: 0 };
  };
  const r = await b.send({ epoch:b.epoch, peer, request_id: 'confirmation-race', text: 'test' }, nativePeer(peer));
  assert.equal(r.state, 'confirmed'); assert.equal(r.message_id, '90071992547409933');
});
test('NOSEQ is not promoted to confirmation', async t => {
  const { b } = fixture(t);
  b.clients.add({ subscribed: true, destroyed: true });
  const r = await b.send({ epoch:b.epoch, peer, request_id: 'noseq', text: 'test' }, nativePeer(peer));
  b.messages([{ msgId: '90071992547409933', guildId: r.native_id, peerUid: 'u_self', chatType: 1, senderUin: '123', sendStatus: 3, elements: [] }]);
  assert.equal(r.state, 'unknown');
});
test('native rejections and transport failures have different states', async t => {
  const { b } = fixture(t);
  b.msg.sendMsg = async () => ({ result: 42, errMsg: 'rejected' });
  assert.equal((await b.send({ epoch:b.epoch, peer, request_id: 'reject', text: 'test' }, nativePeer(peer))).state, 'failed');
  b.msg.sendMsg = async () => { throw new Error('connection lost'); };
  assert.equal((await b.send({ epoch:b.epoch, peer, request_id: 'lost', text: 'test' }, nativePeer(peer))).state, 'unknown');
});
test('recent snapshot preserves native order, folder entries, exact unread and pin flags', async t => {
  const { b } = fixture(t);
  const make = (id, type) => ({ contactId: id, chatType: type, peerUid: id, peerUin: id, peerName: id, unreadCnt: '999', topFlag: 1 });
  b.recent = { getRecentContactListSnapShot: async () => ({ info: { sortedContactList: ['folder', 'chat'], changedList: [make('chat', 1), { ...make('folder', 7), nestedChangedList: [make('child', 2)], nestedSortedContactList: ['child'] }] } }) };
  b.views.set(1,{sortedContactList:['folder','chat']});
  b.views.set(2,{sortedContactList:['child']});
  b.contacts.set('child',make('child',2));
  const s = await b.snapshot();
  assert.deepEqual(s.conversations.map(c => c.id), ['folder', 'chat']);
  assert.equal(s.conversations[0].children[0].id, 'child');
  assert.equal(s.conversations[1].unread, '999');
  assert.equal(s.conversations[1].pinned, true);
});
test('stale account calls never reach a native method', async t => {
  const { b, count } = fixture(t);
  await assert.rejects(b.call('messages.send', { account: '456', peer, request_id: 'wrong-account', text: 'no' }, {}), /Account changed/);
  assert.equal(count(), 0);
});
test('account switch during asynchronous preparation prevents sending',async t=>{
  const {b,count}=fixture(t);
  b.msg.getMsgsByMsgId=async()=>{
    b.account='456';
    return {msgList:[{msgId:'1',msgSeq:'1',senderUin:'123',senderUid:'u_self',msgTime:'1',msgClientSeq:'1'}]};
  };
  await assert.rejects(b.send({account:'123',epoch:b.epoch,peer,request_id:'switch-during-preparation',text:'test',reply_to:'1'},nativePeer(peer)),/Account changed/);
  assert.equal(count(),0);
});
test('device images use native file elements and device replies are rejected before sending',async t=>{
  const {b,count,dir}=fixture(t);const file=path.join(dir,'image.png');fs.writeFileSync(file,'fixture');
  const device={chat_type:8,uid:'u_device'};let elements;
  b.msg.sendMsg=async(_id,_peer,value)=>{elements=value;return{result:0};};
  const r=await b.send({account:'123',epoch:b.epoch,peer:device,request_id:'device-file',attachments:[{path:file,kind:'image',width:1,height:1}]},nativePeer(device));
  assert.equal(elements.length,1);assert.equal(elements[0].elementType,3);assert.equal(r.state,'submitted');
  await assert.rejects(b.send({account:'123',epoch:b.epoch,peer:device,request_id:'device-reply',text:'no',reply_to:'1'},nativePeer(device)),/do not support replies/);
  assert.equal(count(),0);
});
test('native recent windows retain their absolute order when paging',t=>{
  const {b}=fixture(t);
  b.applyRecentViews([{listType:1,firstContactPos:'0',sortedContactList:['a','b'],changedList:[],moreDataAtTheBottom:true}]);
  b.applyRecentViews([{listType:1,firstContactPos:'1',sortedContactList:['b','c'],changedList:[],moreDataAtTheBottom:false}]);
  assert.deepEqual(b.views.get(1).sortedContactList,['a','b','c']);
  assert.equal(b.views.get(1).firstContactPos,'0');
  b.applyRecentViews([{listType:1,firstContactPos:'0',sortedContactList:['c','a'],changedList:[],moreDataAtTheBottom:false}]);
  assert.deepEqual(b.views.get(1).sortedContactList,['c','a']);
});

test('closing the frontend retains the native recent projection for an unchanged resubscription',t=>{
 const {b}=fixture(t);b.contacts.set('a',{contactId:'a'});b.views.set(1,{sortedContactList:['a']});
 b.unsubscribe();assert.equal(b.contacts.size,1);assert.equal(b.views.get(1).sortedContactList[0],'a');
 b.unsubscribe(true);assert.equal(b.contacts.size,0);assert.equal(b.views.size,0);
});
test('profiles use the native coreInfo and baseInfo fields',async t=>{
 const {b}=fixture(t);b.session.getProfileService=()=>({getCoreAndBaseInfo:async()=>new Map([['u_member',{coreInfo:{uin:'90071992547409933',nick:'名字',remark:'备注'},baseInfo:{longNick:'签名'}}]])});
 const profile=await b.call('profiles.info',{account:'123',uid:'u_member'},{});
 assert.equal(profile.uin,'90071992547409933');assert.equal(profile.nick,'名字');assert.equal(profile.signature,'签名');
});

test('uncached group member profiles are obtained through the native detail query',async t=>{
 const {b}=fixture(t);let count=0;
 b.session.getProfileService=()=>({getCoreAndBaseInfo:async()=>new Map(),fetchUserDetailInfo:async()=>{count++;return {result:0,detail:new Map([['u_member',{simpleInfo:{coreInfo:{uin:'42',nick:'成员'},baseInfo:{longNick:'状态'}}}]])};}});
 const profile=await b.call('profiles.info',{account:'123',uid:'u_member'},{});
 assert.equal(count,1);assert.equal(profile.nick,'成员');assert.equal(b.profileWaiters.size,0);
});

test('history queries navigate around a native anchor in both directions',async t=>{
 const {b}=fixture(t);const requests=[];const raw=id=>({msgId:String(id),msgSeq:String(id),msgTime:String(id),chatType:1,peerUid:peer.uid,elements:[]});
 b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[raw(3)]});
 b.msg.getMsgs=async(_peer,id,count,older)=>{requests.push({id,count,older});return {result:0,msgList:older?[raw(2),raw(1)].slice(0,count):[raw(4),raw(5)].slice(0,count)};};
 const around=await b.call('messages.history',{account:'123',peer,around:'3',limit:5},{});
 assert.equal(around.anchor_found,true);assert.deepEqual(new Set(around.messages.map(m=>m.id)),new Set(['1','2','3','4','5']));
 assert.equal(requests[0].older,true);assert.equal(requests[1].older,false);
 const after=await b.call('messages.history',{account:'123',peer,after:'3',limit:5},{});
 assert.equal(requests.at(-1).older,false);assert.equal(after.has_after,false);
 await assert.rejects(b.call('messages.history',{account:'123',peer,before:'3',after:'3'},{}),/one decimal/);
});
test('a missing native anchor falls back to the current latest page',async t=>{
 const {b}=fixture(t);b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[]});b.msg.getAioFirstViewLatestMsgs=async()=>({result:0,msgList:[]});
 const page=await b.call('messages.history',{account:'123',peer,around:'3',limit:5},{});assert.equal(page.anchor_found,false);assert.equal(page.has_after,false);
});

test('native listeners are installed at login before a frontend subscribes',t=>{
 const {b}=fixture(t);let subscribed=0;
 b.session.getMsgService=()=>b.msg;b.session.getRecentContactService=()=>({});
 b.subscribe=()=>{subscribed++;b.active=true;};
 b.ready({selfUin:'123',selfUid:'u_self'});
 assert.equal(subscribed,1);assert.equal(b.clients.size,0);assert.equal(b.active,true);
 assert.equal(JSON.parse(fs.readFileSync(path.join(path.dirname(b.journal),"active-account.json"),"utf8")).account,"123");
});

test('account changes rotate the native context and old listeners do not reactivate on returning',t=>{
 const {b}=fixture(t);const callbacks=[];let updates=0;
 b.clients.add({subscribed:true,destroyed:true});b.messages=()=>{updates++;};
 b.msg.addKernelMsgListener=handler=>{callbacks.push(handler);};
 b.recent={addKernelRecentContactListener:()=>{}};
 b.session.getMsgService=()=>b.msg;b.session.getRecentContactService=()=>b.recent;
 b.session.getGroupService=()=>({addKernelGroupListener:()=>{}});
 b.session.getProfileService=()=>({addKernelProfileListener:()=>{}});
 b.subscribe();const first=b.epoch;
 b.ready({selfUin:'456',selfUid:'u_b'});const second=b.epoch;
 b.ready({selfUin:'123',selfUid:'u_self'});
 assert.notEqual(first,second);assert.notEqual(b.epoch,first);assert.notEqual(b.epoch,second);
 callbacks[0].onRecvMsg([]);assert.equal(updates,0);
 callbacks.at(-1).onRecvMsg([]);assert.equal(updates,1);
});
test('an epoch change during message preparation prevents native submission',async t=>{
 const {b,count}=fixture(t);const epoch=b.epoch;
 b.msg.getMsgsByMsgId=async()=>{b.epoch='different';return {result:0,msgList:[{msgId:'1',msgSeq:'1',senderUin:'123',senderUid:'u_self',msgTime:'1',clientSeq:'1'}]};};
 await assert.rejects(b.send({account:'123',epoch,peer,request_id:'epoch-during-preparation',text:'test',reply_to:'1'},nativePeer(peer)),/Account changed/);
 assert.equal(count(),0);
});
test('mutating calls reject a stale expected context while duplicate receipts remain idempotent',async t=>{
 const {b,count}=fixture(t);let reads=0;b.msg.setMsgRead=async()=>{reads++;return{result:0};};
 await assert.rejects(b.call('messages.read',{account:'123',peer,epoch:'old'},{}),/context changed/);assert.equal(reads,0);
 const rejected=await b.call('messages.send',{account:'123',peer,epoch:'old',request_id:'stale-context',text:'no'},{});assert.equal(rejected.state,'failed');assert.equal(count(),0);
 const p={account:'123',epoch:b.epoch,peer,request_id:'known',text:'test'};
 const receipt=await b.send(p,nativePeer(peer));b.epoch='new-context';
 assert.equal((await b.send(p,nativePeer(peer))).native_id,receipt.native_id);assert.equal(count(),1);
});
test('business replies are rejected if the account context changes before the response',async t=>{
 const {b}=fixture(t);const replies=[];const socket={destroyed:false,writableLength:0,write:line=>replies.push(JSON.parse(line))};
 b.call=async()=>{b.epoch='new-context';return{messages:[]};};
 await b.handle(socket,JSON.stringify({jsonrpc:'2.0',id:1,method:'messages.history',params:{account:'123',peer}}));
 assert.ok(replies[0].error);assert.match(replies[0].error.message,/Account changed/);
});
test('account changes cancel old recent requests without deleting the next account request',async t=>{
 const {b}=fixture(t);b.recent={fetchAndSubscribeABatchOfRecentContact:()=>{}};
 b.session.getMsgService=()=>b.msg;b.session.getRecentContactService=()=>b.recent;b.subscribe=()=>{b.active=true;};
 const old=b.ensureView(1);const cancelled=assert.rejects(old,/context changed/);
 b.ready({selfUin:'456',selfUid:'u_b'});const current=b.ensureView(1);
 await cancelled;assert.equal(b.viewRequests.size,1);
 b.applyRecentViews([{listType:1,firstContactPos:'0',sortedContactList:[],changedList:[],moreDataAtTheBottom:false}]);
 await current;assert.equal(b.viewRequests.size,0);assert.equal(b.viewCancels.size,0);
});

test('faces retain native text/emoji order and normalize slash or bracket fallback names', t => {
  const {dir} = fixture(t);
  fs.writeFileSync(path.join(dir,'14.png'),'asset');
  for (const name of ['/大冤种','[大冤种]','[/大冤种]']) {
    const m=message({msgId:'1',chatType:1,peerUid:'u_self',elements:[
      {elementId:'a',textElement:{content:'before'}},
      {elementId:'b',faceElement:{faceIndex:14,faceText:name}},
      {elementId:'c',textElement:{content:'after'}},
    ]},'123',new QqResources({emojiDir:dir}));
    assert.equal(m.text,'before[大冤种]after');
    assert.deepEqual(m.parts,[{type:'text',text:'before'},{type:'attachment',element_id:'b'},{type:'text',text:'after'}]);
    const fallback=message({elements:[{faceElement:{faceIndex:99,faceText:'[认可]'}}]},'123',new QqResources({emojiDir:dir}));
    assert.equal(fallback.text,'[认可]');assert.deepEqual(fallback.parts,[{type:'text',text:'[认可]'}]);
  }
});
test('merged records preserve XML cards and only expand validated native parent chains', async t => {
  const {b}=fixture(t);
  const root={msgId:'10',chatType:1,peerUid:'u_self',elements:[{elementId:'a',elementType:16,multiForwardMsgElement:{xmlContent:'<msg><item><title>聊天记录</title><title>A &amp; B</title></item><summary>查看2条消息</summary></msg>'}}]};
  const nested={...root,msgId:'11'};
  const child={msgId:'12',chatType:0,peerUid:'forwarded-sender',elements:[{textElement:{content:'older forwarded body'}}]};
  const card=message(root,'123');assert.equal(card.text,'[合并转发聊天记录]');assert.ok(card.forward.includes('A &amp; B'));
  let calls=[];
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[root]});
  b.msg.getMultiMsg=async(_p,root,id)=>{calls.push([root,id]);return{result:0,msgList:id==='10'?[nested,child]:[child]};};
  const expanded=await b.call('messages.forward',{account:'123',peer,path:['10','11']},{});
  assert.deepEqual(calls,[['10','10'],['10','11']]);
  assert.equal(expanded.messages[0].text,'older forwarded body');assert.deepEqual(expanded.messages[0].peer,peer);
  await assert.rejects(b.call('messages.forward',{account:'123',peer,path:['10','999']},{}),/unavailable/);
  await assert.rejects(b.call('messages.forward',{account:'123',peer,path:Array(9).fill('10')},{}),/1..8/);
  const recalled=message({...root,elements:[...root.elements,{grayTipElement:{subElementType:1}}]},'123');
  assert.equal(recalled.forward,null);assert.deepEqual(recalled.parts,[]);
});

test('faces also resolve QQ downloaded animated and static resources', t => {
  const {dir}=fixture(t);
  fs.mkdirSync(path.join(dir,'apng'));fs.mkdirSync(path.join(dir,'static'));
  fs.writeFileSync(path.join(dir,'apng','s400.png'),'animated face');
  fs.writeFileSync(path.join(dir,'static','s401.png'),'static face');
  for (const id of [400,401]) {
    const m=message({elements:[{elementId:'1',faceElement:{faceIndex:id,faceText:'[认可]'}}]},'123',new QqResources({emojiResources:[dir]}));
    assert.equal(m.text,'[认可]');assert.equal(m.attachments.length,1);
    assert.equal(m.parts[0].type,'attachment');
  }
});

test('market faces render as one image, use the pinned QQ CDN, and cache concurrent downloads', async t => {
  const {b,dir}=fixture(t),id='0123456789abcdef0123456789abcdef';
  const raw={msgId:'1',chatType:1,peerUid:'u_self',elements:[{elementId:'2',marketFaceElement:{emojiId:id,faceName:'[认可]'}}]};
  const m=message(raw,'123');assert.equal(m.text,'[认可]');assert.equal(m.attachments[0].kind,'sticker');
  assert.deepEqual(m.parts,[{type:'attachment',element_id:'2'}]);
  const originalFetch=global.fetch;t.after(()=>{global.fetch=originalFetch;});let calls=0;
  global.fetch=async(url)=>{calls++;assert.equal(url,`https://gxh.vip.qq.com/club/item/parcel/item/01/${id}/raw300.gif`);return new Response(Buffer.from('GIF89a-test'));};
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[raw]});
  const [a,c]=await Promise.all([b.call('media.resolve',{account:'123',peer,id:'1',element_id:'2'},{}),b.call('media.resolve',{account:'123',peer,id:'1',element_id:'2'},{})]);
  assert.equal(a.path,c.path);assert.equal(calls,1);assert.ok(a.path.startsWith(dir));
  const cached=message(raw,'123',b.resources);assert.equal(cached.attachments[0].path,a.path);
  await b.resources.marketFace(id);assert.equal(calls,1);
  await assert.rejects(b.resources.marketFace('../secret'),/Invalid/);
});

test('reaction aggregates and targeted gray notifications remain structured native facts', t => {
  const {b}=fixture(t);
  const resources=b.resources;resources.faces.set('76',{name:'赞'});
  const raw={msgId:'1',msgSeq:'9007199254740993',chatType:2,peerUid:'100',elements:[{textElement:{content:'body'}}],emojiLikesList:[{emojiId:'76',emojiType:'1',likesCnt:'9007199254740995',isClicked:'1'}]};
  const value=message(raw,'123',resources);
  assert.equal(value.reactions[0].count,'9007199254740995');assert.equal(value.reactions[0].clicked,true);assert.equal(value.reactions[0].label,'赞');
  const xml='<gtip><qq jp="123" nm="Alice"/><url msgseq="9007199254740993"/><face id="76"/></gtip>';
  const notice=message({...raw,elements:[{grayTipElement:{xmlElement:{templId:'10382',content:xml}}}]},'123',resources);
  assert.equal(notice.reaction_notice,xml);assert.equal(notice.text,'[表情回应]');assert.ok(!notice.text.includes('[系统消息]'));
  let events=[];b.clients.add({subscribed:true});b.event=(kind,value)=>events.push({kind,value});
  b.messages([{...raw,elements:undefined}]);
  assert.deepEqual(events.map(e=>e.kind),['reactions.updated']);
});
test('reactions use exact native sequence, explicit emoji type and desired state, with paginated members', async t => {
  const {b}=fixture(t);const group={chat_type:2,uid:'100'};
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[{msgId:'1',msgSeq:'9007199254740993',chatType:2,peerUid:'100',elements:[]}]});
  const calls=[];
  b.msg.setMsgEmojiLikes=async(...args)=>{calls.push(args);return {result:0};};
  b.msg.getMsgEmojiLikesList=async(...args)=>{calls.push(args);return {result:0,emojiLikesList:[{tinyId:'9007199254740999',nickName:'Alice'}],cookie:'next',isLastPage:false};};
  const params={account:'123',epoch:b.epoch,peer:group,id:'1',emoji_id:'76',emoji_type:'1',set:true};
  await b.call('reactions.set',params,{});assert.deepEqual(calls[0].slice(1),['9007199254740993','76','1',true]);
  const page=await b.call('reactions.members',{...params,cookie:'prior'},{});
  assert.equal(page.members[0].uid,'9007199254740999');assert.equal(page.cookie,'next');assert.deepEqual(calls[1].slice(1),['9007199254740993','76','1','prior',false,50]);
  await assert.rejects(b.call('reactions.set',{...params,peer},{}),/group/);
  await assert.rejects(b.call('reactions.set',{...params,epoch:'stale'},{}),/session changed|Session context changed/);
  await assert.rejects(b.call('reactions.set',{...params,emoji_type:'0'},{}),/Invalid reaction/);
});
test('sticker catalogs use native favorites and a stale account cannot populate the new account cache', async t => {
  const {b,dir}=fixture(t);const file=path.join(dir,'image.gif');fs.writeFileSync(file,'GIF89a');
  b.resources.faces.set('14',{id:'14',name:'微笑',native:{faceIndex:14,faceType:1}});
  b.msg.fetchFavEmojiList=async()=>({result:0,emojiInfoList:[{md5:'abc',emoId:1,emoPath:file,modifyWord:'favorite'}]});
  const catalog=await b.call('stickers.list',{account:'123'},{});assert.deepEqual(catalog.items.map(s=>s.kind),['sticker','face']);
  assert.equal((await b.call('stickers.resolve',{account:'123',id:'favorite:abc'},{})).path,file);
  let done;b.msg.fetchFavEmojiList=()=>new Promise(resolve=>{done=resolve;});
  const pending=b.call('stickers.list',{account:'123'},{});b.account='456';b.epoch='next';b.stickers.clear();done({result:0,emojiInfoList:[{md5:'old'}]});
  await assert.rejects(pending,/session changed/);assert.equal(b.stickers.size,0);
});
test('face and custom sticker sends share durable deduplication and use native element types', async t => {
  const {b,dir}=fixture(t);b.resources.faces.set('14',{native:{faceIndex:14,faceType:1,faceText:'/微笑'}});
  const sent=[];b.msg.sendMsg=async(_id,_peer,elements)=>{sent.push(elements);return {result:0};};
  const p={account:'123',peer,epoch:b.epoch,request_id:'face-send',text:'',attachments:[{kind:'face',element_id:'14'}]};
  await Promise.all([b.send(p,nativePeer(peer)),b.send(p,nativePeer(peer))]);assert.equal(sent.length,1);assert.equal(sent[0][0].elementType,6);
  const file=path.join(dir,'custom.gif');fs.writeFileSync(file,'GIF89a');
  b.msg.getRichMediaFilePathForGuild=()=>file;
  await b.send({...p,request_id:'sticker-send',attachments:[{kind:'sticker',path:file,width:1,height:1}]},nativePeer(peer));
  assert.equal(sent[1][0].picElement.picSubType,1);assert.equal(sent[1][0].picElement.original,true);
});

test('a session change during reaction preflight never calls a mutation on the next account', async t => {
  const {b}=fixture(t);let release,mutations=0;
  b.msg.getMsgsByMsgId=()=>new Promise(resolve=>{release=resolve;});
  b.msg.setMsgEmojiLikes=()=>{mutations++;return {result:0};};
  const pending=b.call('reactions.set',{account:'123',epoch:b.epoch,peer:{chat_type:2,uid:'100'},id:'1',emoji_id:'76',emoji_type:'1',set:true},{});
  b.account='456';b.epoch='changed';
  release({result:0,msgList:[{msgId:'1',msgSeq:'1',elements:[]}]});
  await assert.rejects(pending,/session changed/);assert.equal(mutations,0);
});
test('resource metadata errors are non-fatal and legacy asset caches remain readable', async t => {
  const {dir}=fixture(t);const emojiDir=path.join(dir,'base');fs.mkdirSync(emojiDir);fs.writeFileSync(path.join(emojiDir,'default_config.json'),'{truncated');
  const cache=path.join(dir,'new-cache');const legacy=path.join(dir,'market-faces');fs.mkdirSync(legacy);
  const id='0123456789abcdef0123456789abcdef';const file=path.join(legacy,`${id}.gif`);fs.writeFileSync(file,'GIF89a');
  const resources=new QqResources({emojiDir,stateDir:dir,cacheDir:cache});assert.equal(resources.errors.length,1);assert.equal(await resources.marketFace(id),file);
  fs.writeFileSync(path.join(emojiDir,'default_config.json'),JSON.stringify({normalPanelResult:{SysEmojiGroupList:[{SysEmojiList:[{emojiId:'14',describe:'/微笑'}]}]}}));
  resources.reload();assert.equal(resources.errors.length,0);assert.equal(resources.faces.get('14').name,'微笑');
});
test('reaction-only native updates cannot turn an unconfirmed send into a failed receipt', t => {
  const {b}=fixture(t);b.clients.add({subscribed:true});b.event=()=>{};
  b.receipts.set('123:pending',{account:'123',peer:{chat_type:2,uid:'100'},native_id:'1',state:'submitted'});
  b.messages([{msgId:'1',chatType:2,peerUid:'100',emojiLikesList:[]}]);
  assert.equal(b.receipts.get('123:pending').state,'submitted');
});

test('mutations require an expected host epoch and reactions reject a different native peer', async t => {
  const {b}=fixture(t);let count=0;b.msg.setMsgRead=()=>{count++;};
  await assert.rejects(b.call('messages.read',{account:'123',peer},{}),/context changed/);assert.equal(count,0);
  const result=await b.call('messages.send',{account:'123',peer,request_id:'missing-epoch',text:'x'},{});
  assert.equal(result.state,'failed');
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[{msgId:'1',msgSeq:'1',chatType:2,peerUid:'other-group',elements:[]}]});
  b.msg.setMsgEmojiLikes=()=>{count++;};
  await assert.rejects(b.call('reactions.set',{account:'123',epoch:b.epoch,peer:{chat_type:2,uid:'100'},id:'1',emoji_id:'76',emoji_type:'1',set:true},{}),/unavailable/);
  assert.equal(count,0);
});
test('unroutable native deltas invalidate state without emitting malformed messages', t => {
  const {b}=fixture(t);b.clients.add({subscribed:true});const events=[];b.event=(kind)=>events.push(kind);
  b.messages([{msgId:'1',emojiLikesList:[]}]);assert.deepEqual(events,['messages.invalidated']);
});

test('native references reject overflow and malformed IDs before calling C++ services', async t => {
  const {b}=fixture(t);let lookups=0;b.msg.getMsgsByMsgId=()=>{lookups++;throw new Error('unexpected native call');};
  for (const id of ['18446744073709551616','-1','x','01']) {
    await assert.rejects(b.call('messages.get',{account:'123',peer,ids:[id]},{}),/IDs required/);
    await assert.rejects(b.call('media.resolve',{account:'123',peer,id,element_id:'1'},{}),/IDs required/);
  }
  assert.equal(lookups,0);
});
test('media downloads are single-flight, bounded, and canceled on session changes', async t => {
  const {b}=fixture(t);let downloads=0;
  b.msg.getMsgsByMsgId=async(_peer,ids)=>({result:0,msgList:[{msgId:ids[0],chatType:1,peerUid:'u_self',elements:[{elementId:'1',picElement:{sourcePath:''}}]}]});
  b.msg.downloadRichMedia=()=>{downloads++;};
  const first=b.resolve({id:'1',element_id:'1'},nativePeer(peer));
  const second=b.resolve({id:'1',element_id:'1'},nativePeer(peer));
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(downloads,1);assert.equal(b.mediaWaiters.size,1);
  b.unsubscribe(true);
  await assert.rejects(first,/session changed/);await assert.rejects(second,/session changed/);assert.equal(b.mediaWaiters.size,0);
  b.msg.downloadRichMedia=()=>{throw new Error('native download rejected');};
  await assert.rejects(b.resolve({id:'2',element_id:'1'},nativePeer(peer)),/native download rejected/);assert.equal(b.mediaWaiters.size,0);
});

test('bundled QQ emoji metadata preserves animated sticker fields when shared resources are not installed', t => {
  const {dir}=fixture(t);fs.writeFileSync(path.join(dir,'default_config.json'),JSON.stringify({normalPanelResult:{SysEmojiGroupList:[{SysEmojiList:[{emojiId:'311',describe:'/打call',emojiType:1,aniStickerId:1,aniStickerPackId:1}]}]}}));
  const resources=new QqResources({emojiDir:dir});const face=resources.faces.get('311');
  assert.equal(face.native.faceType,3);assert.equal(face.native.stickerId,1);assert.equal(face.native.packId,1);assert.equal(face.native.stickerType,1);
});
test('reaction capability follows the native single-message lookup',async t=>{
  const {b}=fixture(t);const peer={chat_type:2,uid:'group'};
  const raw={msgId:'100',msgSeq:'12',chatType:2,peerUid:'group',elements:[{textElement:{content:'original'}}]};
  b.msg.setMsgEmojiLikes=async()=>({result:0});b.msg.getMsgEmojiLikesList=async()=>({result:0});
  assert(!b.status().capabilities.includes('reactions'));
  b.msg.getSingleMsg=async(p,seq)=>{assert.equal(p.peerUid,'group');assert.equal(seq,'12');return{result:0,msgList:[raw,{...raw,msgSeq:'13'}]};};
  assert(b.status().capabilities.includes('reactions'));
  assert.equal((await b.call('reactions.refresh',{account:'123',peer,seq:'12'},{})).messages[0].text,'original');
  b.msg.getSingleMsg=async()=>{b.account='changed';return{result:0,msgList:[raw]};};
  await assert.rejects(b.call('reactions.refresh',{account:'123',peer,seq:'12'},{}),/session changed/);
});

test('account instances reuse native data and reject duplicate account sessions before init', t=>{
  const {AccountSession}=require('../bridge/accounts.cjs');
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'qqtui-accounts-'));
  t.after(()=>fs.rmSync(dir,{recursive:true,force:true}));
  const root=path.join(dir,'original QQ');fs.mkdirSync(root);
  const db=path.join(root,'existing.db');fs.writeFileSync(db,'existing chat data');
  const make=account=>{const paths={userData:root};const session=new AccountSession({getPath:key=>paths[key],setPath:(key,value)=>{paths[key]=value;}},{account,profile:path.join(dir,'profiles',account),leaseDir:path.join(dir,'leases')});assert.equal(paths.appData,session.profile);return session;};
  const one=make('123'),two=make('456'),duplicate=make('123');
  const config=id=>({selfUin:id,desktopPathConfig:{account_path:path.join(dir,'temporary'),other:7},secret:'unchanged'});
  assert.notEqual(one.engine({global_path_config:{}}).global_path_config.desktopGlobalPath,two.login({}).commonPath);
  assert.notEqual(one.login({}).commonPath,path.join(root,'global'));
  const first=one.prepare(config('123'));
  assert.equal(first.desktopPathConfig.account_path,root);
  assert.equal(first.desktopPathConfig.other,7);assert.equal(first.secret,'unchanged');
  assert.equal(two.prepare(config('456')).desktopPathConfig.account_path,root);
  assert.doesNotThrow(()=>one.prepare(config('123')));
  assert.throws(()=>duplicate.prepare(config('123')),/另一个宿主/);
  assert.throws(()=>two.prepare(config('123')),/仅登录 456/);
  assert.equal(fs.readFileSync(db,'utf8'),'existing chat data');
  one.close();assert.doesNotThrow(()=>duplicate.prepare(config('123')));
  two.close();duplicate.close();
  assert.equal(fs.readdirSync(path.join(dir,'leases')).length,0);
});

test('account catalog returns only display fields from native remembered logins',async t=>{
  const {b}=fixture(t);
  b.wrapper={NodeIKernelLoginService:{get:()=>({getLoginList:async()=>({result:0,LocalLoginInfoList:[{uin:'456',nickName:'name',uid:'private_uid',password:'never exposed'}]})})}};
  assert.deepEqual(await b.call('accounts.list',{},{}),{items:[{account:'456',name:'name'}]});
});

test('forwarded media lookup follows its record path and preserves native download routing',async t=>{
  const {b,dir}=fixture(t);
  const file=path.join(dir,'forward.png');fs.writeFileSync(file,'test image');
  const root={msgId:'10',chatType:1,peerUid:'u_self',elements:[{multiForwardMsgElement:{xmlContent:'<msg/>'}}]};
  const child={msgId:'12',chatType:2,peerUid:'789',elements:[{elementId:'8',picElement:{fileName:'image.png',sourcePath:file}}]};
  b.msg.getMsgsByMsgId=async(_peer,ids)=>({result:0,msgList:ids[0]==='10'?[root]:[]});
  b.msg.getMultiMsg=async(_peer,rootId,id)=>{assert.equal(rootId,'10');assert.equal(id,'10');return {result:0,msgList:[child]};};
  const expanded=await b.call('messages.forward',{account:'123',peer,path:['10']},{});
  assert.deepEqual(expanded.messages[0].forward_path,['10']);
  assert.equal((await b.call('media.resolve',{account:'123',peer,id:'12',element_id:'8',forward_path:['10']},{})).path,file);
  await assert.rejects(b.call('media.resolve',{account:'123',peer,id:'12',element_id:'8'},{}),/unavailable/);
  await assert.rejects(b.call('media.resolve',{account:'123',peer,id:'12',element_id:'8',forward_path:['99']},{}),/unavailable/);
  fs.unlinkSync(file);
  b.msg.downloadRichMedia=args=>{assert.equal(args.msgId,'10');assert.equal(args.elementId,'8');assert.equal(args.chatType,1);assert.equal(args.peerUid,'u_self');assert.equal(args.filePath,file);fs.writeFileSync(file,'downloaded');b.mediaWaiters.get('10:8').finish(null,{filePath:file});};
  assert.equal((await b.call('media.resolve',{account:'123',peer,id:'12',element_id:'8',forward_path:['10']},{})).path,file);
  fs.unlinkSync(file);
  b.msg.getMultiMsg=async(_peer,rootId,id)=>{
    assert.equal(rootId,'10');
    return {result:0,msgList:id==='10'?[{msgId:'11',elements:[{multiForwardMsgElement:{xmlContent:'<msg/>'}}]}]:[child]};
  };
  assert.equal((await b.call('media.resolve',{account:'123',peer,id:'12',element_id:'8',forward_path:['10','11']},{})).path,file);
});

test('gray tips use structured notice semantics and reaction targets resolve by native sequence',async t=>{
  const {b}=fixture(t);
  const poke=message({msgId:'10',elements:[{grayTipElement:{subElementType:17,jsonGrayTipElement:{jsonStr:JSON.stringify({align:'center',items:[{txt:'甲'},{txt:' 拍了拍 '},{txt:'乙'}]})}}}]},'123');
  assert.equal(poke.system_notice,true);assert.equal(poke.text,'甲 拍了拍 乙');
  assert.equal(message({msgId:'11',elements:[{textElement:{content:'撤回了消息'}}]},'123').system_notice,false);
  b.msg.getSingleMsg=async(_peer,seq)=>({result:0,msgList:[{msgId:'99',msgSeq:seq,chatType:1,peerUid:'u_self',elements:[{textElement:{content:'original'}}]}]});
  const found=await b.call('messages.sequence',{account:'123',peer,seq:'987654'},{});
  assert.equal(found.messages[0].id,'99');assert.equal(found.messages[0].text,'original');
});

test('forwarded images retain QQ supplied URL authentication and share a bounded trusted download',async t=>{
  const {b,dir}=fixture(t);let fetches=0;
  const origin='https://gchat.qpic.cn/download?appid=1407&fileid=native-file&rkey=native-key';
  t.mock.method(globalThis,'fetch',async url=>{fetches++;assert.equal(String(url),origin);return new Response(Buffer.from('GIF89a'),{status:200});});
  const image={md5HexStr:'0123456789abcdef0123456789abcdef',originImageUrl:origin,sourcePath:path.join(dir,'not-cached.jpg'),fileName:'image.gif'};
  const root={msgId:'10',chatType:1,peerUid:'u_self',elements:[{multiForwardMsgElement:{xmlContent:'<msg/>'}}]};
  const child={msgId:'12',chatType:2,peerUid:'284840486',elements:[{elementId:'8',picElement:image}]};
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[root]});
  b.msg.getMultiMsg=async()=>({result:0,msgList:[child]});
  b.session.getRichMediaService=()=>{throw new Error('Do not replace a QQ supplied picture URL with a file download');};
  const params={account:'123',peer,id:'12',element_id:'8',forward_path:['10']};
  const [first,second]=await Promise.all([b.call('media.resolve',params,{}),b.call('media.resolve',params,{})]);
  assert.equal(first.path,second.path);assert.equal(fetches,1);assert.ok(fs.existsSync(first.path));
  assert.equal(message(child,'123',b.resources).attachments[0].path,first.path);
  await assert.rejects(b.resources.image({...image,md5HexStr:'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',originImageUrl:'https://127.0.0.1/private'}),/Untrusted/);
  assert.equal(fetches,1);
});

test('a stale forwarded image URL falls back to native download and creates its QQ supplied parent',async t=>{
  const {b,dir}=fixture(t);let attempts=0;
  const file=path.join(dir,'native','images','picture.png');
  const root={msgId:'10',chatType:1,peerUid:'u_self',elements:[{multiForwardMsgElement:{xmlContent:'<msg/>'}}]};
  const child={msgId:'12',chatType:2,peerUid:'789',elements:[{elementId:'8',picElement:{fileName:'picture.png',sourcePath:file,md5HexStr:'0123456789abcdef0123456789abcdef',originImageUrl:'https://gchat.qpic.cn/expired'}}]};
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[root]});
  b.msg.getMultiMsg=async()=>({result:0,msgList:[child]});
  b.resources.image=async()=>{attempts++;throw new Error('HTTP 404');};
  b.msg.downloadRichMedia=args=>{
    assert.equal(args.msgId,'10');
    assert.equal(args.elementId,'8');
    fs.writeFileSync(file,'native picture');
    b.mediaWaiters.get('10:8').finish(null,{filePath:file});
    return {result:0};
  };
  const result=await b.call('media.resolve',{account:'123',peer,id:'12',element_id:'8',forward_path:['10']},{});
  assert.equal(result.path,file);assert.equal(attempts,1);
});

test('QQ legacy picture references use the upstream CDN grammar without resource-key services',async t=>{
  const {dir}=fixture(t);const resources=new QqResources({stateDir:dir});let fetches=0;
  const md5=require('node:crypto').createHash('md5').update('GIF89a').digest('hex');
  t.mock.method(globalThis,'fetch',async url=>{fetches++;assert.equal(String(url),`https://gchat.qpic.cn/gchatpic_new/0/0-0-${md5.toUpperCase()}/0`);return new Response(Buffer.from('GIF89a'),{status:200});});
  const file=await resources.image({md5HexStr:md5,originImageUrl:'/download?appid=1407&fileid=native-reference'});
  assert.ok(fs.existsSync(file));assert.equal(fetches,1);
});

test('mail reminder cards expose readable fields and an official mail link, without card tokens', () => {
  const mail={title:'Sender',subTitle:'Subject',content:'Summary',mailUrl:'https://wx.mail.qq.com/list/readtemplate?mailid=test'};
  const raw={chatType:103,peerUid:'u_mail',msgId:'11',elements:[{arkElement:{bytesData:JSON.stringify({view:'mail',meta:{mail},config:{token:'private-token'}})}}]};
  const m=message(raw,'123');
  assert.equal(m.text,'Sender\nSubject\nSummary\n'+mail.mailUrl);
  assert.equal(m.parts[0].text,m.text);
  assert.ok(!m.text.includes('private-token'));
  mail.mailUrl='https://attacker.invalid/mail';
  raw.elements[0].arkElement.bytesData=JSON.stringify({view:'mail',meta:{mail}});
  assert.ok(!message(raw,'123').text.includes('attacker'));
  raw.elements[0].arkElement.bytesData='{bad json';
  assert.equal(message(raw,'123').text,'[QQ 邮箱提醒]');
});
test('conversation read validates native contact identity and handles mailbox and group assistant',async t=>{
  const {b}=fixture(t);const reads=[];const clears=[];
  b.contacts.set('mail',{chatType:103,peerUid:'u_mail'});
  b.contacts.set('folder',{chatType:7,peerUid:''});
  b.contacts.set('child',{chatType:2,peerUid:'42',unreadCnt:'4'});
  b.contacts.set('empty',{chatType:2,peerUid:'43',unreadCnt:'0'});
  b.views.set(2,{sortedContactList:['child','empty']});
  b.snapshot=async()=>({});b.ensureView=async()=>{};b.completeView=async()=>{};
  b.recent={clearMsgUnreadCount:async p=>{clears.push(p);}};
  b.msg.setMsgRead=async p=>{reads.push(p);return{result:0}};
  const params={account:'123',epoch:b.epoch};
  await b.call('conversations.read',{...params,id:'mail',peer:{chat_type:103,uid:'u_mail'}},{});
  assert.deepEqual(clears,[]);
  assert.deepEqual(reads,[{chatType:103,peerUid:'u_mail',guildId:''}]);
  await b.call('messages.read',{...params,peer:{chat_type:103,uid:'u_mail'}},{});
  assert.deepEqual(reads[1],reads[0]);
  await b.call('conversations.read',{...params,id:'folder',peer:{chat_type:7,uid:''}},{});
  assert.deepEqual(reads[2],{chatType:2,peerUid:'42',guildId:''});
  assert.deepEqual(clears,['folder']);
  b.msg.setMsgRead=async()=>({result:5,errMsg:'native read failed'});
  await assert.rejects(b.call('messages.read',{...params,peer:{chat_type:103,uid:'u_mail'}},{}),/native read failed/);
  await assert.rejects(b.call('conversations.read',{...params,id:'mail',peer:{chat_type:103,uid:'u_mail'}},{}),/native read failed/);
  await assert.rejects(b.call('conversations.read',{...params,id:'mail',peer:{chat_type:103,uid:'wrong'}},{}),/Conversation changed/);
  await assert.rejects(b.call('conversations.read',{...params,epoch:'stale',id:'mail',peer:{chat_type:103,uid:'u_mail'}},{}),/context/);
  await assert.rejects(b.call('messages.send',{...params,peer:{chat_type:103,uid:'u_mail'}},{}),/read-only/);
});

test('expired original images fall back to a labelled cached thumbnail and preserve actionable errors',async t=>{
  const {b,dir}=fixture(t);const small=path.join(dir,'small.png'),large=path.join(dir,'large.png');
  fs.writeFileSync(small,'small');fs.writeFileSync(large,'larger-thumbnail');
  const pic={sourcePath:'',thumbPath:new Map([[1,small],[2,large]])};
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[{msgId:'1',chatType:1,peerUid:'u_self',elements:[{elementId:'2',picElement:pic}]}]});
  b.msg.downloadRichMedia=()=>{throw new Error('2006021: srv_err_code=[-5503042] File is Invalid')};
  b.resources.image=async()=>{throw new Error('HTTP 404')};
  const result=await b.resolve({id:'1',element_id:'2'},nativePeer(peer));
  assert.equal(result.path,large);assert.match(result.warning,/仅显示 QQ 缓存的缩略图/);assert.match(result.warning,/-5503042/);
  pic.thumbPath.clear();
  await assert.rejects(b.resolve({id:'1',element_id:'2'},nativePeer(peer)),/HTTP 404.*重试/);
});
test('native notification decisions preserve important mentions in quiet and assistant groups',t=>{
  const {b}=fixture(t);let callback;const events=[];
  b.clients.add({subscribed:true,destroyed:true});b.event=(kind,data)=>events.push({kind,...data});
  b.recent={addKernelRecentContactListener:object=>{callback=object;}};
  b.msg.addKernelMsgListener=()=>{};
  b.session.getGroupService=()=>({addKernelGroupListener:()=>{}});
  b.session.getProfileService=()=>({addKernelProfileListener:()=>{}});
  b.subscribe();
  callback.onMsgUnreadCountUpdate(new Map([[1,{listType:1,msgListUnreadCnt:'5'}]]));assert.equal(b.unreadCounts.get(1),'5');
  const contact={chatType:103,peerUid:'mail',senderUin:'100',msgId:'10',isOnlineMsg:true,notifiedType:2,unreadCnt:'1',peerName:'QQ邮箱提醒',abstractContent:[{content:'new mail'}]};
  const emit=c=>callback.onRecentContactNotification([c],{msgListUnreadCnt:'8'},0);
  const alerts=()=>events.filter(e=>e.kind==='notification');
  emit({...contact,isOnlineMsg:false});assert.equal(alerts().length,0);
  emit(contact);assert.equal(alerts().length,1);assert.match(alerts()[0].notification.items[0].title,/^\[8\] QQ · 123 · QQ邮箱提醒$/);
  assert.deepEqual(alerts()[0].notification.items[0].target,{account:'123',peer:{chat_type:contact.chatType,uid:contact.peerUid},id:String(contact.msgId)});
  emit(contact);emit({...contact,msgId:'11',senderUin:'123'});
  emit({...contact,msgId:'12',isMsgDisturb:true,notifiedType:1});
  assert.equal(alerts().length,1);
  b.views.set(2,{sortedContactList:['nested']});
  for(const shieldFlag of ['2','4']) {
    const group={...contact,chatType:2,peerUid:'quiet-'+shieldFlag,contactId:'nested',shieldFlag,isMsgDisturb:true,unreadCnt:'0'};
    emit({...group,msgId:'20',notifiedType:1});
    const before=alerts().length;
    emit({...group,msgId:'21',atType:1,notifiedType:2});
    assert.equal(alerts().length,before+1,'native important @ must survive group mute, assistant membership and zero unread count');
  }
  assert.equal(alerts().length,3);
  const enabled={...contact,chatType:2,peerUid:'enabled',msgId:'30',shieldFlag:'1'};
  emit(enabled);assert.equal(alerts().length,4);
  emit({...enabled,msgId:'31',shieldFlag:'4',notifiedType:1});
  assert.equal(alerts().length,4,'quiet ordinary messages only update counts');
  emit({...enabled,msgId:'32',shieldFlag:'4',atType:2,notifiedType:2});
  assert.equal(alerts().length,5,'native important @all uses the same native decision');
  emit({...enabled,msgId:'33',notifiedType:undefined});
  emit({...enabled,msgId:'34',notifiedType:99});
  assert.equal(alerts().length,5,'unknown native decisions do not create alerts');
});

test('upload preparation creates only the native target directory and reports a missing source before submission',async t=>{
  const {b,dir,count}=fixture(t);const source=path.join(dir,'clipboard.png');fs.writeFileSync(source,'image bytes');const destination=path.join(dir,'native','nested','image.png');
  b.msg.getRichMediaFilePathForGuild=()=>destination;
  const request={account:'123',epoch:b.epoch,peer,request_id:'directory-copy',attachments:[{kind:'image',path:source,width:2,height:2}]};
  await b.send(request,nativePeer(peer));assert.equal(fs.readFileSync(destination,'utf8'),'image bytes');assert.equal(count(),1);
  b.msg.getRichMediaFilePathForGuild=()=>{fs.unlinkSync(source);return destination;};
  const failed=await b.call('messages.send',{...request,request_id:'missing-source'},{});assert.equal(failed.state,'failed');assert.match(failed.error,/重新选择附件/);assert.equal(count(),1);
});

test('reaction sequence lookup excludes gray tips sharing the target sequence and other peers', async t => {
  const {b}=fixture(t);const group={chat_type:2,uid:'100'};
  const original={msgId:'2',msgSeq:'42',chatType:2,peerUid:'100',elements:[{textElement:{content:'original'}}]};
  const tip={...original,msgId:'1',elements:[{grayTipElement:{xmlElement:{templId:'10382',content:'<gtip><url msgseq="42"/></gtip>'}}}]};
  b.msg.getSingleMsg=async(p,seq)=>{assert.equal(seq,'42');return {result:0,msgList:[tip,{...original,msgId:'3',peerUid:'other'},original]};};
  const result=await b.call('reactions.refresh',{account:'123',peer:group,seq:'42'},{});
  assert.deepEqual(result.messages.map(m=>m.id),['2']);
});

test('native group names resolve reaction actors, poke participants and recall operators with shared bounded queries', async t => {
  const {b}=fixture(t);let queries=0;
  b.session.getGroupService=()=>({getAllMemberList:async(_id,force)=>{assert.equal(force,true);queries++;return {result:{infos:new Map([
    ['u_actor',{uin:'456',cardName:'群名片',nick:'昵称'}],['u_target',{uin:'789',nick:'朋友'}],
  ])}};}});
  const base={chatType:2,peerUid:'100',senderUid:'u_target',senderUin:'789'};
  const raws=[
    {...base,msgId:'1',elements:[{grayTipElement:{xmlElement:{templId:'10382',content:'<gtip><qq uin="u_actor" jp="456"/><url msgseq="42"/></gtip>'}}}]},
    {...base,msgId:'2',elements:[{grayTipElement:{jsonGrayTipElement:{recentAbstract:'456拍了拍789',jsonStr:JSON.stringify({items:[{type:'qq',uid:'u_actor',txt:'456'},{txt:'拍了拍'},{type:'qq',uid:'u_target',txt:'789'}]})}}}]},
    {...base,msgId:'3',elements:[{grayTipElement:{subElementType:1,revokeElement:{operatorUid:'u_actor',operatorNick:'456'}}}]},
  ];
  const [messages]=await Promise.all([b.normalizeMessages(raws),b.normalizeMessages(raws)]);
  assert.equal(queries,1);assert.equal(messages[0].notice_names.u_actor,'群名片');
  assert.equal(messages[1].text,'群名片拍了拍朋友');assert.equal(messages[2].text,'群名片撤回了一条消息');
  await b.normalizeMessages(raws);assert.equal(queries,1);
  b.presentation.member('100',new Map());await b.normalizeMessages(raws);assert.equal(queries,2);
  b.presentation.close();assert.equal(b.presentation.nameLists.size,0);
});

test('group recall delegates other senders to QQ and preserves native rejection', async t => {
  const {b}=fixture(t);const group={chat_type:2,uid:'100'};let calls=0;
  b.msg.recallMsg=async(p,ids)=>{assert.equal(p.peerUid,'100');assert.deepEqual(ids,['42']);calls++;return {result:0};};
  await b.call('messages.recall',{account:'123',epoch:b.epoch,peer:group,id:'42'},{});
  assert.equal(calls,1);
  b.msg.recallMsg=async()=>({result:42,errMsg:'native permission denied'});
  await assert.rejects(b.call('messages.recall',{account:'123',epoch:b.epoch,peer:group,id:'42'},{}),/native permission denied/);
});

test('reaction refresh cannot cross an account change during the native fetch', async t => {
  const {b}=fixture(t);let lookedUp=false;
  b.msg.getSingleMsg=async()=>{b.account='456';return {result:0};};
  b.msg.getMsgsBySeqList=async()=>{lookedUp=true;return {result:0,msgList:[]};};
  await assert.rejects(b.call('reactions.refresh',{account:'123',peer:{chat_type:2,uid:'100'},seq:'42'},{}),/session changed/);
  assert.equal(lookedUp,false);
});

test('missing original messages never resolve to a gray tip or a neighboring sequence',async t=>{
  const {b}=fixture(t);const peer={chat_type:2,uid:'100'};
  const original={msgId:'2',msgSeq:'42',chatType:2,peerUid:'100',elements:[{textElement:{content:'body'}}]};
  for (const rows of [[],[{...original,msgSeq:'41'},{...original,msgId:'1',elements:[{grayTipElement:{xmlElement:{templId:'10382'}}}]}]]) {
    b.msg.getSingleMsg=async()=>({result:0,msgList:rows});
    assert.deepEqual((await b.call('messages.sequence',{account:'123',peer,seq:'42'},{})).messages,[]);
  }
});

test('image uploads use GIF bytes and matching native media subtypes rather than the filename', async t => {
  const cases = [
    ['GIF89a', 'favorite.jpg', 'sticker', 2000, 1],
    ['GIF87a', 'favorite', 'sticker', 2000, 1],
    ['GIF89a', 'picture.gif', 'image', 2000, 0],
    ['\x89PNG\r\n', 'favorite.gif', 'sticker', 1000, 1],
    ['\xff\xd8\xff\xe0JF', 'picture.jpg', 'image', 1000, 0],
  ];
  for (const [header, name, kind, type, subtype] of cases) {
    const {b,dir}=fixture(t);const source=path.join(dir,name),target=path.join(dir,'upload',name);
    fs.writeFileSync(source,Buffer.from(header,'latin1'));
    b.msg.getRichMediaFilePathForGuild=args=>{
      assert.equal(args.elementType,2);assert.equal(args.elementSubType,subtype);
      return target;
    };
    b.msg.sendMsg=async(_id,_peer,elements)=>{
      const picture=elements[0].picElement;
      assert.equal(picture.picType,type);assert.equal(picture.picSubType,subtype);
      assert.equal(picture.sourcePath,target);assert.deepEqual(fs.readFileSync(target),fs.readFileSync(source));
      return {result:0};
    };
    const receipt=await b.send({account:'123',epoch:b.epoch,peer,request_id:'image-format',attachments:[{path:source,name,kind,width:2,height:2}]},nativePeer(peer));
    assert.equal(receipt.state,'submitted');
  }
});

test('viewer leases are connection scoped and status updates work without subscribers', async t => {
  const { b } = fixture(t);
  const calls = [];
  b.options.desktop = {
    setReady: ready => calls.push(['ready', ready]),
    viewer: (client, active) => calls.push([client, active]),
  };
  b.phase = 'login';
  b.event('status', {status:b.status()});
  const client = {};
  await b.call('host.viewer', {active:true}, client);
  await b.call('host.viewer', {active:false}, client);
  await assert.rejects(b.call('host.viewer', {active:'yes'}, client), /Viewer state required/);
  assert.deepEqual(calls, [['ready',false],[client,true],[client,false]]);
});

test('closing the viewer RPC connection releases its desktop lease', async t => {
  const {b}=fixture(t);
  const leases=new Set();
  let release;
  const released=new Promise(resolve=>{release=resolve;});
  b.options.desktop={
    viewer(client,active) {
      if(active) leases.add(client);
      else { leases.delete(client); release(); }
    },
  };
  b.listen();
  const {once}=require('node:events');
  await once(b.server,'listening');
  const socket=require('node:net').createConnection(b.socketPath);
  t.after(async()=>{socket.destroy();await new Promise(resolve=>b.server.close(resolve));});
  await once(socket,'connect');
  socket.write(JSON.stringify({jsonrpc:'2.0',id:1,method:'host.viewer',params:{active:true}})+'\n');
  await once(socket,'data');
  assert.equal(leases.size,1);
  socket.end();
  await released;
  assert.equal(leases.size,0);
});

test('type-10 cards render readable fields and links without exposing JSON or executing app URLs', () => {
  const raw={msgId:'10',chatType:2,peerUid:'group',elements:[{elementType:1,textElement:{content:'前文'}},{elementType:10,arkElement:{bytesData:JSON.stringify({app:'com.tencent.miniapp_01',meta:{detail_1:{title:'视频分享',desc:'标题\u001b[31m',qqdocurl:'https://example.com/video',url:'javascript:alert(1)',preview:'https://example.com/private-preview',appid:'private-app-id'}}})}}]};
  const normalized=message(raw,'123');
  assert.deepEqual(normalized.card_parts,[1]);
  assert.equal(normalized.parts[1].type,'text');
  assert(normalized.text.includes('视频分享\n标题[31m\nhttps://example.com/video'));
  assert(!normalized.text.includes('javascript:') && !normalized.text.includes('private-preview') && !normalized.text.includes('private-app-id'));
  raw.elements=[{elementType:10,arkElement:{bytesData:'invalid'}}];
  assert.equal(message(raw,'123').text,'[卡片消息]');
});
test('JSON merged-forward cards use the native forward reader', async t => {
  const {b}=fixture(t);
  const card={app:'com.tencent.multimsg',meta:{detail:{news:[{text:'甲: 内容'}],summary:'查看1条消息'}}};
  const raw={msgId:'10',chatType:1,peerUid:'u_self',elements:[{elementType:10,arkElement:{bytesData:JSON.stringify(card)}}]};
  b.msg.getMsgsByMsgId=async()=>({result:0,msgList:[raw]});
  b.msg.getMultiMsg=async(_peer,root,id)=>{assert.equal(root,'10');assert.equal(id,'10');return {result:0,msgList:[{msgId:'11',chatType:1,peerUid:'u_self',elements:[{textElement:{content:'原消息'}}]}]};};
  assert.equal(message(raw,'123').forward,JSON.stringify(card));
  assert.equal((await b.call('messages.forward',{account:'123',peer,path:['10']},{})).messages[0].text,'原消息');
});
test('native unread anchors use the full peer and preserve sequence and message ID namespaces', async t => {
  const {b}=fixture(t),group={chat_type:2,uid:'group'},seq='90071992547409931',id='18446744073709551613';
  b.msg.getFirstUnreadMsgSeq=async p=>{assert.deepEqual(p,nativePeer(group));return {result:0,seq};};
  b.msg.getSingleMsg=async(p,value)=>{assert.equal(value,seq);return {result:0,msgList:[{chatType:2,peerUid:'group',msgSeq:seq,msgId:id}]};};
  assert.deepEqual(await b.call('conversations.unread',{account:'123',peer:group},{}),{id});
  b.msg.getSingleMsg=async()=>({result:0,msgList:[]});
  b.msg.getMsgsBySeqAndCount=async(p,value,count,desc,reverse)=>{
    assert.equal(value,seq);assert.equal(count,1);assert.equal(desc,false);assert.equal(reverse,false);
    return {result:0,msgList:[{chatType:2,peerUid:'group',msgSeq:'90071992547409932',msgId:id}]};
  };
  assert.deepEqual(await b.call('conversations.unread',{account:'123',peer:group},{}),{id});
  b.msg.getFirstUnreadMsgSeq=async()=>({result:0,seq:'0'});
  assert.deepEqual(await b.call('conversations.unread',{account:'123',peer:group},{}),{id:null});
  b.msg.getFirstUnreadMsgSeq=async()=>({result:5,errMsg:'unavailable'});
  await assert.rejects(b.call('conversations.unread',{account:'123',peer:group},{}),/unavailable/);
});
