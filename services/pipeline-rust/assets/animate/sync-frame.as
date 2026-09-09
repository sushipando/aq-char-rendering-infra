if(!(this is MovieClip) || parent == null || !(parent is MovieClip)) { return; }
if(MovieClip(this).currentFrame != MovieClip(parent).currentFrame - this.parentFirstFrame) {
    if(this.parentFirstFrame != undefined) { gotoAndPlay(MovieClip(parent).currentFrame - this.parentFirstFrame); }
    this.___applyLayerZdepthAndEffects___();
}
