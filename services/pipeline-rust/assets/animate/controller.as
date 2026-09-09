package BoAEnergy_fla
{
   import adobe.utils.*;
   import flash.accessibility.*;
   import flash.desktop.*;
   import flash.display.*;
   import flash.errors.*;
   import flash.events.*;
   import flash.external.*;
   import flash.filters.*;
   import flash.geom.*;
   import flash.globalization.*;
   import flash.media.*;
   import flash.net.*;
   import flash.net.drm.*;
   import flash.printing.*;
   import flash.profiler.*;
   import flash.sampler.*;
   import flash.sensors.*;
   import flash.system.*;
   import flash.text.*;
   import flash.text.engine.*;
   import flash.text.ime.*;
   import flash.ui.*;
   import flash.utils.*;
   import flash.xml.*;
   
   [Embed(source="/_assets/assets.swf", symbol="symbol12")]
   public dynamic class Symbol5_5 extends MovieClip
   {
      
      public const CAMERA:Number = 0;
      
      public const LAYER_PROPERTIES:Number = 1;
      
      public const LAYER_OBJECT:Number = 2;
      
      public function Symbol5_5()
      {
         super();
         addFrameScript(0,this.frame1);
      }
      
      public function ___applyLayerZdepthAndEffects___() : *
      {
         var _loc2_:* = undefined;
         var _loc1_:* = 0;
         while(_loc1_ < numChildren)
         {
            _loc2_ = getChildAt(_loc1_);
            if(_loc2_ != null && _loc2_ is MovieClip && MovieClip(_loc2_).hasOwnProperty("containerType") && MovieClip(_loc2_).containerType == this.LAYER_PROPERTIES)
            {
               _loc2_.executeFrame();
            }
            _loc1_++;
         }
      }
      
      public function ___onAdded___(param1:Event) : *
      {
         var _loc4_:int = 0;
         var _loc5_:* = undefined;
         var _loc6_:* = undefined;
         var _loc2_:* = param1.target;
         if(!_loc2_)
         {
            return;
         }
         var _loc3_:* = _loc2_.parent;
         if(Boolean(_loc3_) && _loc3_ == this)
         {
            if(_loc2_ is MovieClip)
            {
               _loc4_ = 0;
               while(_loc4_ < numChildren)
               {
                  _loc5_ = getChildAt(_loc4_);
                  if(_loc5_ != _loc2_)
                  {
                     if(_loc5_ is MovieClip && _loc5_.name == _loc2_.name)
                     {
                        removeChild(_loc2_);
                        break;
                     }
                  }
                  _loc4_++;
               }
            }
         }
         else if(_loc2_ is MovieClip)
         {
            _loc6_ = param1.target.parent;
            if(_loc6_ is MovieClip && _loc6_.containertype == this.LAYER_OBJECT && _loc6_.parent != null)
            {
               _loc6_.parent[param1.target.name] = param1.target;
            }
         }
      }
      
      internal function frame1() : *
      {
         this.___applyLayerZdepthAndEffects___();
         addEventListener(Event.ADDED,this.___onAdded___);
      }
   }
}

