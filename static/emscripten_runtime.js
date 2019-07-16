
var Module = {
    'noInitialRun': false,
    'print': function(text) { 
        console.log('stdout: ' + text);
    },
    'printErr': function(text) { 
        console.error('stderr: ' + text);
    },
    'setStatus': (text) => {
        console.log('status: ' + text);
    }
};