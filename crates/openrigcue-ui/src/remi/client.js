
                var Remi = function() {
                this._pendingSendMessages = [];
                this._ws = null;
                this._comTimeout = null;
                this._failedConnections = 0;
                this._openSocket();
                };

                // from http://stackoverflow.com/questions/5515869/string-length-in-bytes-in-javascript
                // using UTF8 strings I noticed that the javascript .length of a string returned less
                // characters than they actually were
                Remi.prototype._byteLength = function(str) {
                    // returns the byte length of an utf8 string
                    return str.length;
                };

                Remi.prototype._paramPacketize = function (ps){
                    var ret = '';
                    for (var pkey in ps) {
                        if( ret.length>0 )ret = ret + '|';
                        var pstring = pkey+'='+ps[pkey];
                        var pstring_length = this._byteLength(pstring);
                        pstring = pstring_length+'|'+pstring;
                        ret = ret + pstring;
                    }
                    return ret;
                };

                Remi.prototype._openSocket = function(){
                    var ws_wss = "ws";
                    try{
                        ws_wss = document.location.protocol.startsWith('https')?'wss':'ws';
                    }catch(ex){}

                    var self = this;
                    try{
                        host = '%(host)s'
                        if (host !== ''){
                            wss_url = `${ws_wss}://${host}/`
                        }
                        else{
                            host = document.location.host;
                            pathname = document.location.pathname;
                            wss_url = `${ws_wss}://${document.location.host}${pathname}`;
                        }

                        this._ws = new WebSocket(wss_url);
                        console.debug('opening websocket');

                        this._ws.onopen = function(evt){
                            if(self._ws.readyState == 1){
                                self._ws.send('connected');

                                try {
                                    document.getElementById("loading").style.display = 'none';
                                } catch(err) {
                                    console.log('Error hiding loading overlay ' + err.message);
                                }

                                self._failedConnections = 0;

                                while(self._pendingSendMessages.length>0){
                                    self._ws.send(self._pendingSendMessages.shift()); /*without checking ack*/
                                }
                            }
                            else{
                                console.debug('onopen fired but the socket readyState was not 1');
                            }
                        };

                        this._ws.onmessage = function(evt){
                            var received_msg = evt.data;

                            if( received_msg[0]=='0' ){ /*show_window*/
                                var index = received_msg.indexOf(',')+1;
                                /*var idRootNodeWidget = received_msg.substr(0,index-1);*/
                                var content = received_msg.substr(index,received_msg.length-index);

                                document.body.innerHTML = decodeURIComponent(content);
                            }else if( received_msg[0]=='1' ){ /*update_widget*/
                                var focusedElement=-1;
                                var caretStart=-1;
                                var caretEnd=-1;
                                if (document.activeElement)
                                {
                                    focusedElement = document.activeElement.id;
                                    try{
                                        caretStart = document.activeElement.selectionStart;
                                        caretEnd = document.activeElement.selectionEnd;
                                    }catch(e){console.debug(e.message);}
                                }
                                var index = received_msg.indexOf(',')+1;
                                var idElem = received_msg.substr(1,index-2);
                                var content = received_msg.substr(index,received_msg.length-index);

                                var elem = document.getElementById(idElem);
                                try{
                                    elem.insertAdjacentHTML('afterend',decodeURIComponent(content));
                                    elem.parentElement.removeChild(elem);
                                }catch(e){
                                    /*Microsoft EDGE doesn't support insertAdjacentHTML for SVGElement*/
                                    var ns = document.createElementNS("http://www.w3.org/2000/svg",'tmp');
                                    ns.innerHTML = decodeURIComponent(content);
                                    elem.parentElement.replaceChild(ns.firstChild, elem);
                                    console.debug(e.message);
                                }

                                var elemToFocus = document.getElementById(focusedElement);
                                if( elemToFocus != null ){
                                    elemToFocus.focus();
                                    try{
                                        elemToFocus = document.getElementById(focusedElement);
                                        if(caretStart>-1 && caretEnd>-1) elemToFocus.setSelectionRange(caretStart, caretEnd);
                                    }catch(e){console.debug(e.message);}
                                }
                            }else if( received_msg[0]=='2' ){ /*javascript*/
                                var content = received_msg.substr(1,received_msg.length-1);
                                try{
                                    eval(content);
                                }catch(e){console.debug(e.message);};
                            }else if( received_msg[0]=='3' ){ /*ack*/
                                self._pendingSendMessages.shift() /*remove the oldest*/
                                if(self._comTimeout!==null)
                                    clearTimeout(self._comTimeout);
                            }
                        };

                        this._ws.onclose = function(evt){
                            /* websocket is closed. */
                            console.debug('Connection is closed... event code: ' + evt.code + ', reason: ' + evt.reason);
                            // Some explanation on this error: http://stackoverflow.com/questions/19304157/getting-the-reason-why-websockets-closed
                            // In practice, on a unstable network (wifi with a lot of traffic for example) this error appears
                            // Got it with Chrome saying:
                            // WebSocket connection to 'ws://x.x.x.x:y/' failed: Could not decode a text frame as UTF-8.
                            // WebSocket connection to 'ws://x.x.x.x:y/' failed: Invalid frame header

                            try {
                                document.getElementById("loading").style.display = '';
                            } catch(err) {
                                console.log('Error hiding loading overlay ' + err.message);
                            }

                            self._failedConnections += 1;

                            console.debug('failed connections=' + self._failedConnections + ' queued messages=' + self._pendingSendMessages.length);

                            if(self._failedConnections > 3) {

                                // check if the server has been restarted - which would give it a new websocket address,
                                // new state, and require a reload
                                console.debug('Checking if GUI still up ' + location.href);

                                var http = new XMLHttpRequest();
                                http.open('HEAD', location.href);
                                http.onreadystatechange = function() {
                                    if (http.status == 200) {
                                        // server is up but has a new websocket address, reload
                                        location.reload();
                                    }
                                };
                                http.send();

                                self._failedConnections = 0;
                            }

                            if(evt.code == 1006){
                                self._renewConnection();
                            }
                        };

                        this._ws.onerror = function(evt){
                            /* websocket is closed. */
                            /* alert('Websocket error...');*/
                            console.debug('Websocket error... event code: ' + evt.code + ', reason: ' + evt.reason);
                        };

                    }catch(ex){this._ws=false;alert('websocketnot supported or server unreachable');}
                }


                /*this uses websockets*/
                Remi.prototype.sendCallbackParam = function (widgetID,functionName,params /*a dictionary of name:value*/){
                    var paramStr = '';
                    if(params!==null) paramStr=this._paramPacketize(params);
                    var message = encodeURIComponent(unescape('callback' + '/' + widgetID+'/'+functionName + '/' + paramStr));
                    this._pendingSendMessages.push(message);
                    if( this._pendingSendMessages.length < %(max_pending_messages)s ){
                        if (this._ws !== null && this._ws.readyState == 1)
                            this._ws.send(message);
                            if(this._comTimeout===null)
                                this._comTimeout = setTimeout(this._checkTimeout, %(messaging_timeout)s);
                    }else{
                        console.debug('Renewing connection, this._ws.readyState when trying to send was: ' + this._ws.readyState)
                        this._renewConnection();
                    }
                };

                /*this uses websockets*/
                Remi.prototype.sendCallback = function (widgetID,functionName){
                    this.sendCallbackParam(widgetID,functionName,null);
                };

                Remi.prototype._renewConnection = function(){
                    // ws.readyState:
                    //A value of 0 indicates that the connection has not yet been established.
                    //A value of 1 indicates that the connection is established and communication is possible.
                    //A value of 2 indicates that the connection is going through the closing handshake.
                    //A value of 3 indicates that the connection has been closed or could not be opened.
                    if( this._ws.readyState == 1){
                        try{
                            this._ws.close();
                        }catch(err){};
                    }
                    else if(this._ws.readyState == 0){
                    // Don't do anything, just wait for the connection to be stablished
                    }
                    else{
                        this._openSocket();
                    }
                };

                Remi.prototype._checkTimeout = function(){
                    if(this._pendingSendMessages.length > 0)
                        this._renewConnection();
                };

                window.onerror = function(message, source, lineno, colno, error) {
                    var params={};params['message']=message;
                    params['source']=source;
                    params['lineno']=lineno;
                    params['colno']=colno;
                    params['error']=JSON.stringify(error);
                    remi.sendCallbackParam('%(emitter_identifier)s','%(event_name)s',params);
                    return false;
                };
                
                window.remi = new Remi();
