(self.webpackChunk_N_E=self.webpackChunk_N_E||[]).push([[695],{69850:function(e,r,t){(window.__NEXT_P=window.__NEXT_P||[]).push(["/cluster",function(){return t(60613)}])},60613:function(e,r,t){"use strict";t.r(r),t.d(r,{default:function(){return M}});var n=t(47568),s=t(34051),i=t.n(s),a=t(85893),u=t(40639),c=t(53812),o=t(66479),l=t(96486),d=t(9008),p=t.n(d),h=t(67294),f=t(9253),m=t(77975),x=t(3023),v=t(75358),w=t(83235),j=t(5330),y=t(40965),g=t(69333);function b(){return k.apply(this,arguments)}function k(){return(k=(0,n.Z)(i().mark((function e(){var r;return i().wrap((function(e){for(;;)switch(e.prev=e.next){case 0:return e.next=2,g.Z.get("/api/metrics/cluster");case 2:return r=e.sent,e.abrupt("return",r);case 4:case"end":return e.stop()}}),e)})))).apply(this,arguments)}function C(){return N.apply(this,arguments)}function N(){return(N=(0,n.Z)(i().mark((function e(){var r;return i().wrap((function(e){for(;;)switch(e.prev=e.next){case 0:return e.next=2,g.Z.get("/api/clusters/1");case 2:return r=e.sent.map(y.cX.fromJSON),e.abrupt("return",r);case 4:case"end":return e.stop()}}),e)})))).apply(this,arguments)}function _(){return S.apply(this,arguments)}function S(){return(S=(0,n.Z)(i().mark((function e(){var r;return i().wrap((function(e){for(;;)switch(e.prev=e.next){case 0:return e.next=2,g.Z.get("/api/clusters/2");case 2:return r=e.sent.map(y.cX.fromJSON),e.abrupt("return",r);case 4:case"end":return e.stop()}}),e)})))).apply(this,arguments)}function Z(e){var r,t,n=e.workerNodeType,s=e.workerNode;return(0,a.jsx)(h.Fragment,{children:(0,a.jsxs)(u.gC,{alignItems:"start",spacing:1,children:[(0,a.jsxs)(u.Ug,{children:[(0,a.jsx)(u.xu,{w:3,h:3,flex:"none",bgColor:"green.600",rounded:"full"}),(0,a.jsxs)(u.xv,{fontWeight:"medium",fontSize:"xl",textColor:"black",children:[n," #",s.id]})]}),(0,a.jsx)(u.xv,{textColor:"gray.500",m:0,children:"Running"}),(0,a.jsxs)(u.xv,{textColor:"gray.500",m:0,children:[null===(r=s.host)||void 0===r?void 0:r.host,":",null===(t=s.host)||void 0===t?void 0:t.port]})]})})}function E(e){var r=e.job,t=e.instance,n=e.metrics,s=e.isCpuMetrics,i=(0,h.useCallback)((function(){var e=[];if(0===n.length)return[];var r=n.at(-1).timestamp,t=!0,s=!1,i=void 0;try{for(var a,u=(0,l.reverse)((0,l.clone)(n))[Symbol.iterator]();!(t=(a=u.next()).done);t=!0){for(var c=a.value;r-c.timestamp>0;)r-=60,e.push({timestamp:r,value:0});e.push(c),r-=60}}catch(o){s=!0,i=o}finally{try{t||null==u.return||u.return()}finally{if(s)throw i}}for(;e.length<60;)e.push({timestamp:r,value:0}),r-=60;return(0,l.reverse)(e)}),[n]);return(0,a.jsx)(h.Fragment,{children:(0,a.jsxs)(u.gC,{alignItems:"start",spacing:1,children:[(0,a.jsxs)(u.xv,{textColor:"gray.500",mx:3,children:[(0,a.jsx)("b",{children:r})," ",t]}),(0,a.jsx)(f.h,{width:"100%",height:100,children:(0,a.jsxs)(m.T,{data:i(),children:[(0,a.jsx)(x.K,{dataKey:"timestamp",type:"number",domain:["dataMin","dataMax"],hide:!0}),s&&(0,a.jsx)(v.B,{type:"number",domain:[0,1],hide:!0}),(0,a.jsx)(w.u,{isAnimationActive:!1,type:"linear",dataKey:"value",strokeWidth:1,stroke:c.rS.colors.teal[500],fill:c.rS.colors.teal[100]})]})})]})})}function M(){var e=(0,h.useState)([]),r=e[0],t=e[1],s=(0,h.useState)([]),c=s[0],d=s[1],f=(0,h.useState)(),m=f[0],x=f[1],v=(0,o.pm)();(0,h.useEffect)((function(){function e(){return(e=(0,n.Z)(i().mark((function e(){return i().wrap((function(e){for(;;)switch(e.prev=e.next){case 0:return e.prev=0,e.t0=t,e.next=4,C();case 4:return e.t1=e.sent,(0,e.t0)(e.t1),e.t2=d,e.next=9,_();case 9:e.t3=e.sent,(0,e.t2)(e.t3),e.next=17;break;case 13:e.prev=13,e.t4=e.catch(0),v({title:"Error Occurred",description:e.t4.toString(),status:"error",duration:5e3,isClosable:!0}),console.error(e.t4);case 17:case"end":return e.stop()}}),e,null,[[0,13]])})))).apply(this,arguments)}return function(){e.apply(this,arguments)}(),function(){}}),[v]),(0,h.useEffect)((function(){function e(){return e=(0,n.Z)(i().mark((function e(){var r;return i().wrap((function(e){for(;;)switch(e.prev=e.next){case 0:return e.prev=1,e.next=4,b();case 4:return(r=e.sent).cpuData=(0,l.sortBy)(r.cpuData,(function(e){return e.metric.instance})),r.memoryData=(0,l.sortBy)(r.memoryData,(function(e){return e.metric.instance})),x(r),e.next=10,new Promise((function(e){return setTimeout(e,5e3)}));case 10:e.next=17;break;case 12:return e.prev=12,e.t0=e.catch(1),v({title:"Error Occurred",description:e.t0.toString(),status:"error",duration:5e3,isClosable:!0}),console.error(e.t0),e.abrupt("break",19);case 17:e.next=0;break;case 19:case"end":return e.stop()}}),e,null,[[1,12]])}))),e.apply(this,arguments)}return function(){e.apply(this,arguments)}(),function(){}}),[v]);var w=(0,a.jsxs)(u.xu,{p:3,children:[(0,a.jsx)(j.Z,{children:"Cluster Overview"}),(0,a.jsxs)(u.rj,{my:3,templateColumns:"repeat(3, 1fr)",gap:6,width:"full",children:[r.map((function(e){return(0,a.jsx)(u.P4,{w:"full",rounded:"xl",bg:"white",shadow:"md",borderWidth:1,p:6,children:(0,a.jsx)(Z,{workerNodeType:"Frontend",workerNode:e})},e.id)})),c.map((function(e){return(0,a.jsx)(u.P4,{w:"full",rounded:"xl",bg:"white",shadow:"md",borderWidth:1,p:6,children:(0,a.jsx)(Z,{workerNodeType:"Compute",workerNode:e})},e.id)}))]}),(0,a.jsx)(j.Z,{children:"CPU Usage"}),(0,a.jsx)(u.MI,{my:3,columns:3,spacing:6,width:"full",children:m&&m.cpuData.map((function(e){return(0,a.jsx)(u.P4,{w:"full",rounded:"xl",bg:"white",shadow:"md",borderWidth:1,children:(0,a.jsx)(E,{job:e.metric.job,instance:e.metric.instance,metrics:e.sample,isCpuMetrics:!0})},e.metric.instance)}))}),(0,a.jsx)(j.Z,{children:"Memory Usage"}),(0,a.jsx)(u.MI,{my:3,columns:3,spacing:6,width:"full",children:m&&m.memoryData.map((function(e){return(0,a.jsx)(u.P4,{w:"full",rounded:"xl",bg:"white",shadow:"md",borderWidth:1,children:(0,a.jsx)(E,{job:e.metric.job,instance:e.metric.instance,metrics:e.sample,isCpuMetrics:!1})},e.metric.instance)}))})]});return(0,a.jsxs)(h.Fragment,{children:[(0,a.jsx)(p(),{children:(0,a.jsx)("title",{children:"Cluster Overview"})}),w]})}}},function(e){e.O(0,[662,482,986,836,774,888,179],(function(){return r=69850,e(e.s=r);var r}));var r=e.O();_N_E=r}]);